//! Captured JavaScript intrinsics, bounded JSON conversion and per-cell host calls.

use std::{cell::RefCell, rc::Rc};

use cocoa_core::{
    capability::{CapabilityError, Effects},
    execution::{ContextEffects, FeedbackError, OperationEffects, host_value_fits},
};
use rquickjs::{
    Array, Ctx, Exception, Function, Object, Persistent, Promise, Value, object::Property,
};
use tokio::sync::{mpsc, oneshot};

use crate::{FeedbackLimits, HostRequest};

#[derive(Default)]
pub(crate) struct Bindings {
    pub printed: Vec<serde_json::Value>,
    print_bytes: usize,
    print_truncated: bool,
    pub output_bytes: usize,
    pub feedback: Option<FeedbackLimits>,
    pub observations: Option<mpsc::Sender<String>>,
    pub host: Option<mpsc::Sender<HostRequest>>,
    pending: Vec<Pending>,
    pub failure: Option<FeedbackError>,
    host_failures: Vec<FeedbackError>,
    host_failures_total: u32,
    rejections: Option<Persistent<Function<'static>>>,
    spawn_receipt: Option<Persistent<Function<'static>>>,
    syntax_error_prototype: Option<Persistent<Object<'static>>>,
    refresh: Option<Persistent<Function<'static>>>,
}

impl Bindings {
    pub fn install<'js>(
        shared: &Rc<RefCell<Self>>,
        context: &Ctx<'js>,
        methods: &[String],
    ) -> rquickjs::Result<Function<'js>> {
        if shared.borrow().rejections.is_none() {
            let constructor: Object = context.globals().get("SyntaxError")?;
            let prototype: Object = constructor.get("prototype")?;
            shared.borrow_mut().syntax_error_prototype = Some(Persistent::save(context, prototype));
            // Weak keys preserve retained error identity without rooting discarded errors.
            let rejections: Function = context.eval("(() => { const errors = new WeakMap(), get = WeakMap.prototype.get, set = WeakMap.prototype.set, apply = Reflect.apply; return (value, error) => { if (error === undefined) return apply(get, errors, [value]); apply(set, errors, [value, error]); }; })()")?;
            shared.borrow_mut().rejections = Some(Persistent::save(context, rejections));
        }
        let handles: Object = context.eval(include_str!("handles.js"))?;
        let contains: Function = handles.get("contains")?;
        let receipt: Function = handles.get("receipt")?;
        shared.borrow_mut().spawn_receipt = Some(Persistent::save(context, receipt));
        let boundary: Function = context.eval(include_str!("boundary.js"))?;
        let convert: Function = boundary.call((contains,))?;
        let format: Function = context.eval("(() => { const string = String, slice = String.prototype.slice, character_code = String.prototype.charCodeAt, apply = Reflect.apply; return (value, limit) => { try { const text = string(value); let end = limit; const last = apply(character_code, text, [end - 1]); if (last >= 0xd800 && last <= 0xdbff) end--; return {message: apply(slice, text, [0, end]), truncated: text.length > end}; } catch { return {message:'JavaScript exception', truncated:false}; } }; })()")?;
        let notice_bytes = serde_json::to_vec(&PrintPreview {
            truncated: true,
            json_prefix: None,
        })
        .map_err(|_| Exception::throw_type(context, "print encoding failed"))?
        .len();
        let bindings = shared.clone();
        let print = Function::new(
            context.clone(),
            move |context: Ctx<'js>, value: Value<'js>, invalid: u8, diagnostic: String| {
                let mut bindings = bindings.borrow_mut();
                let limits = bindings
                    .feedback
                    .ok_or_else(|| Exception::throw_type(&context, "cell stopped"))?;
                if invalid == 1 {
                    bindings.failure = Some(FeedbackError::InvalidOutput {
                        message: diagnostic,
                        truncated: false,
                    });
                    return Err(Exception::throw_type(&context, "invalid printed output"));
                }
                if bindings.print_truncated {
                    return Ok(());
                }
                let value = if invalid == 2 {
                    None
                } else {
                    Some(decode(&context, value)?)
                };
                let mut counter = crate::ByteLimit {
                    remaining: limits.printed_bytes,
                };
                let fits = value
                    .as_ref()
                    .is_some_and(|value| serde_json::to_writer(&mut counter, value).is_ok());
                let bytes = limits.printed_bytes - counter.remaining;
                let separator = usize::from(!bindings.printed.is_empty());
                // Keep room for a notice without replacing already observed prints.
                let reserve = if limits.printed_bytes >= notice_bytes + 2 {
                    notice_bytes + 1
                } else {
                    0
                };
                let remaining = limits
                    .printed_bytes
                    .saturating_sub(bindings.print_bytes + separator + 2);
                let value = if !fits
                    || bindings.printed.len() >= limits.print_calls
                    || bytes.saturating_add(reserve) > remaining
                {
                    if remaining < notice_bytes {
                        bindings.failure = Some(FeedbackError::OutputLimit {
                            limit_bytes: limits.printed_bytes as u32,
                        });
                        return Err(Exception::throw_type(
                            &context,
                            "print output limit exceeded",
                        ));
                    }
                    let value = print_preview(&context, value.as_ref(), remaining)?;
                    bindings.print_truncated = true;
                    value
                } else {
                    match value {
                        Some(value) => value,
                        None => unreachable!("only converted values fit the print budget"),
                    }
                };
                let encoded = serde_json::to_string(&value)
                    .map_err(|_| Exception::throw_type(&context, "print encoding failed"))?;
                bindings.print_bytes += encoded.len() + separator;
                let text = match &value {
                    serde_json::Value::String(text) => text.clone(),
                    _ => encoded,
                };
                if let Some(sender) = &bindings.observations {
                    match sender.try_send(text) {
                        Ok(()) => {}
                        Err(error) => tracing::trace!(%error, "print observation dropped"),
                    }
                }
                bindings.printed.push(value);
                Ok(())
            },
        )?;
        let wrap: Function = context.eval("((emit, convert, limit) => { const overflow = {}; return (...values) => { if (!values.length) return; let value; try { if (values.length > 32) throw 'print accepts at most 32 arguments'; value = convert(values.length === 1 ? values[0] : values, limit, true, overflow); } catch (error) { return emit(null, error === overflow ? 2 : 1, typeof error === 'string' ? error : 'unsupported printed value at $'); } emit(value, 0, ''); }; })")?;
        context.globals().set(
            "print",
            wrap.call::<_, Function>((print, convert.clone(), 256 * 1024))?,
        )?;
        let bindings = shared.clone();
        let call = Function::new(
            context.clone(),
            move |context: Ctx<'js>, name: rquickjs::String<'js>, arguments: Value<'js>| {
                let name = name.to_cstring()?;
                if name.len() > 256 {
                    return Err(Exception::throw_type(
                        &context,
                        "capability name limit exceeded",
                    ));
                }
                let arguments =
                    checked_arguments(&context, arguments, bindings.borrow().output_bytes / 2)?;
                let mut bindings = bindings.borrow_mut();
                if bindings.pending.len() >= 16 {
                    bindings.failure = Some(FeedbackError::Javascript {
                        message: "capability bridge limit exceeded".into(),
                        truncated: false,
                    });
                    drop(bindings);
                    return Err(Exception::throw_message(
                        &context,
                        "capability bridge limit exceeded",
                    ));
                }
                let (promise, resolve, reject) = Promise::new(&context)?;
                let (reply, result) = oneshot::channel();
                bindings
                    .host
                    .as_ref()
                    .ok_or_else(|| Exception::throw_type(&context, "cell stopped"))?
                    .try_send(HostRequest {
                        name: name.as_str().into(),
                        arguments,
                        reply,
                    })
                    .map_err(|_| {
                        Exception::throw_type(&context, "host request queue full or closed")
                    })?;
                bindings.pending.push(Pending {
                    method: name.as_str().into(),
                    result,
                    resolve: Persistent::save(&context, resolve),
                    reject: Persistent::save(&context, reject),
                });
                Ok(promise)
            },
        )?;
        let bindings = shared.clone();
        let limit = Function::new(context.clone(), move || bindings.borrow().output_bytes / 2)?;
        let install: Function = context.eval(include_str!("namespaces.js"))?;
        let refresh: Function = install.call((call.clone(), convert.clone(), limit.clone()))?;
        shared.borrow_mut().refresh = Some(Persistent::save(context, refresh));
        let capabilities = Object::new(context.clone())?;
        let wrap: Function = context
            .eval("((call, convert, limit) => { const Type = TypeError; return (name, input) => { if (name === 'cocoa.spawn' || name === 'cocoa.wait' || name === 'cocoa.cancel') throw new Type('management calls require cocoa methods'); let value; try { value = convert(input, limit()); } catch (error) { throw new Type(typeof error === 'string' ? error : 'argument conversion failed'); } return call(name, value); }; })")?;
        capabilities.set(
            "call",
            wrap.call::<_, Function>((call.clone(), convert.clone(), limit.clone()))?,
        )?;
        context.globals().set("capabilities", capabilities)?;
        for (namespace_name, prefix) in [("cocoa", "cocoa."), ("context", "context.")] {
            if !methods.iter().any(|name| name.starts_with(prefix)) {
                continue;
            }
            let namespace = Object::new(context.clone())?;
            namespace.set_prototype(None)?;
            let wrap: Function = context.eval(include_str!("dispatch.js"))?;
            let management: Function = handles.get("wrap")?;
            for name in methods {
                let Some(method) = name.strip_prefix(prefix) else {
                    continue;
                };
                if namespace.contains_key(method)? {
                    continue;
                }
                let wrapper = if matches!(
                    name.as_str(),
                    "cocoa.spawn" | "cocoa.run" | "cocoa.wait" | "cocoa.cancel"
                ) {
                    &management
                } else {
                    &wrap
                };
                let function: Function =
                    wrapper.call((call.clone(), convert.clone(), limit.clone(), name.as_str()))?;
                namespace.prop(method, Property::from(function).enumerable())?;
            }
            let freeze: Function = context.eval("Object.freeze")?;
            freeze.call::<_, ()>((namespace.clone(),))?;
            context
                .globals()
                .prop(namespace_name, Property::from(namespace).enumerable())?;
        }
        Ok(format)
    }

    pub fn refresh(
        shared: &Rc<RefCell<Self>>,
        context: &Ctx<'_>,
        bindings: &[cocoa_core::capability::CapabilityBinding],
    ) -> rquickjs::Result<()> {
        if bindings.len() > 4096 {
            return Err(Exception::throw_type(
                context,
                "generated binding limit exceeded",
            ));
        }
        let entries = Array::new(context.clone())?;
        for (index, binding) in bindings.iter().enumerate() {
            if binding.path.len() != 3 || binding.path[0] != "mcp" || binding.name.len() > 256 {
                return Err(Exception::throw_type(context, "invalid generated binding"));
            }
            let entry = Array::new(context.clone())?;
            entry
                .as_object()
                .prop(0, Property::from(binding.path[1].as_str()))?;
            entry
                .as_object()
                .prop(1, Property::from(binding.path[2].as_str()))?;
            entry
                .as_object()
                .prop(2, Property::from(binding.name.as_str()))?;
            entries
                .as_object()
                .prop(index as u32, Property::from(entry))?;
        }
        let refresh = shared
            .borrow()
            .refresh
            .clone()
            .ok_or_else(|| Exception::throw_type(context, "missing binding installer"))?;
        refresh.restore(context)?.call::<_, ()>((entries,))
    }

    pub fn poll(shared: &Rc<RefCell<Self>>, context: &Ctx<'_>) -> rquickjs::Result<()> {
        let mut index = 0;
        loop {
            let (pending, result) = {
                let mut bindings = shared.borrow_mut();
                if index >= bindings.pending.len() {
                    break;
                }
                let result = match bindings.pending[index].result.try_recv() {
                    Ok(result) => result,
                    Err(oneshot::error::TryRecvError::Empty) => {
                        index += 1;
                        continue;
                    }
                    Err(oneshot::error::TryRecvError::Closed) => {
                        return Err(Exception::throw_type(context, "host broker stopped"));
                    }
                };
                (bindings.pending.swap_remove(index), result)
            };
            let bytes = shared.borrow().output_bytes;
            match result {
                Ok(value) => {
                    if !host_value_fits(&value, bytes) {
                        shared.borrow_mut().failure = Some(FeedbackError::Javascript {
                            message: "host result conversion limit exceeded".into(),
                            truncated: false,
                        });
                        let error = Exception::from_message(
                            context.clone(),
                            "host result conversion limit exceeded",
                        )?;
                        pending.reject.restore(context)?.call::<_, ()>((error,))?;
                        continue;
                    }
                    let value = encode(context, &value)?;
                    let value = if matches!(pending.method.as_str(), "cocoa.spawn" | "cocoa.run") {
                        // Brand receipts before Promise resolution can consult a user-defined then.
                        let receipt = shared.borrow().spawn_receipt.clone().ok_or_else(|| {
                            Exception::throw_type(context, "missing spawn handle registry")
                        })?;
                        match receipt.restore(context)?.call::<_, Value>((value,)) {
                            Ok(handle) => handle,
                            Err(rquickjs::Error::Exception) => {
                                let error = context.catch();
                                pending.reject.restore(context)?.call::<_, ()>((error,))?;
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                    } else {
                        value
                    };
                    pending.resolve.restore(context)?.call::<_, ()>((value,))?;
                }
                Err(error) => {
                    let code = match &error {
                        CapabilityError::Tool { code, .. }
                        | CapabilityError::Context { code, .. } => code.as_str(),
                        CapabilityError::Rejected { .. } => "invalid_arguments",
                        CapabilityError::Backend { .. } => "backend",
                        CapabilityError::Budget => "budget",
                        CapabilityError::PendingLimit => "pending_limit",
                        CapabilityError::OutputLimit => "output_limit",
                        CapabilityError::Cancelled => "cancelled",
                        CapabilityError::Deadline => "deadline",
                        CapabilityError::Unsettled { .. } => "unsettled",
                        CapabilityError::Store(_) => "store",
                    };
                    let message = match &error {
                        CapabilityError::Tool { message, .. }
                        | CapabilityError::Context { message, .. }
                        | CapabilityError::Rejected { message }
                        | CapabilityError::Backend { message, .. } => message.clone(),
                        _ => error.to_string(),
                    };
                    let method = pending.method;
                    let feedback = match &error {
                        CapabilityError::Context { owner, .. } => FeedbackError::Context {
                            method: method.clone(),
                            code: code.into(),
                            message,
                            owner: *owner,
                            operation_effects: ContextEffects::None,
                            truncated: false,
                        },
                        _ => FeedbackError::Capability {
                            method: method.clone(),
                            code: code.into(),
                            message,
                            operation_effects: match error.effects() {
                                Effects::None => OperationEffects::None,
                                Effects::Partial => OperationEffects::Partial,
                                Effects::Unknown => OperationEffects::Unknown,
                            },
                            truncated: false,
                        },
                    };
                    let limits = shared
                        .borrow()
                        .feedback
                        .ok_or_else(|| Exception::throw_type(context, "cell stopped"))?;
                    if matches!(
                        error,
                        CapabilityError::Tool { .. }
                            | CapabilityError::Context { .. }
                            | CapabilityError::Rejected { .. }
                            | CapabilityError::Backend { .. }
                    ) {
                        let mut bindings = shared.borrow_mut();
                        bindings.host_failures_total =
                            bindings.host_failures_total.saturating_add(1);
                        if bindings.host_failures.len() < 8 {
                            bindings
                                .host_failures
                                .push(Self::bounded(feedback.clone(), limits));
                        }
                    }
                    let bounded = Self::bounded(feedback.clone(), limits);
                    let message = match &bounded {
                        FeedbackError::Capability { message, .. }
                        | FeedbackError::Context { message, .. } => message.as_str(),
                        _ => "capability feedback exceeds limit",
                    };
                    let object = Exception::from_message(context.clone(), message)?.into_object();
                    object.prop("code", Property::from(code).enumerable())?;
                    object.prop("method", Property::from(method.as_str()).enumerable())?;
                    let effects = match error.effects() {
                        Effects::None => "none",
                        Effects::Partial => "partial",
                        Effects::Unknown => "unknown",
                    };
                    object.prop("effects", Property::from(effects).enumerable())?;
                    if let CapabilityError::Context { owner, .. } = &error {
                        match owner {
                            Some(owner) => {
                                object.prop(
                                    "owner",
                                    Property::from(i64::from(*owner).to_string()).enumerable(),
                                )?;
                            }
                            None => {
                                object.prop(
                                    "owner",
                                    Property::from(Value::new_null(context.clone())).enumerable(),
                                )?;
                            }
                        }
                    }
                    let value = object.into_value();
                    let rejections =
                        shared.borrow().rejections.clone().ok_or_else(|| {
                            Exception::throw_type(context, "missing error registry")
                        })?;
                    let error = serde_json::to_string(&feedback)
                        .map_err(|_| Exception::throw_type(context, "error encoding failed"))?;
                    rejections
                        .restore(context)?
                        .call::<_, ()>((value.clone(), error))?;
                    pending.reject.restore(context)?.call::<_, ()>((value,))?;
                }
            }
        }
        Ok(())
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn reset(&mut self) {
        *self = Self {
            rejections: self.rejections.take(),
            spawn_receipt: self.spawn_receipt.take(),
            syntax_error_prototype: self.syntax_error_prototype.take(),
            refresh: self.refresh.take(),
            ..Self::default()
        };
    }

    pub fn with_host_failures(
        &self,
        result: Result<(), FeedbackError>,
    ) -> Result<(), FeedbackError> {
        let Some(first) = self.host_failures.first() else {
            return result;
        };
        if self.host_failures_total == 1 && (result.is_ok() || result.as_ref().err() == Some(first))
        {
            return Err(first.clone());
        }
        let mut summary = first.clone();
        if let FeedbackError::Capability {
            message, truncated, ..
        }
        | FeedbackError::Context {
            message, truncated, ..
        }
        | FeedbackError::Javascript { message, truncated } = &mut summary
        {
            let label = if self.feedback.is_some_and(|limits| limits.error_bytes < 128) {
                "failures"
            } else {
                "recoverable host failures"
            };
            message.insert_str(
                0,
                &format!(
                    "{} {label}; earlier effects remain. First: ",
                    self.host_failures_total
                ),
            );
            if let Err(error) = result
                && &error != first
            {
                message.push_str(&format!("; other cell feedback: {error}"));
            }
            for error in self.host_failures.iter().skip(1) {
                message.push_str(&format!("; {error}"));
                let effects = match error {
                    FeedbackError::Capability {
                        operation_effects,
                        truncated,
                        ..
                    } => Some((*operation_effects, *truncated)),
                    FeedbackError::Context { truncated, .. } => {
                        Some((OperationEffects::None, *truncated))
                    }
                    _ => None,
                };
                if let Some((operation_effects, omitted)) = effects {
                    message.push_str(&format!(" (operation effects {operation_effects:?})"));
                    *truncated |= omitted;
                }
            }
            if self.host_failures_total as usize > self.host_failures.len() {
                message.push_str("; further failures omitted, their effects may differ");
                *truncated = true;
            }
        }
        Err(summary)
    }

    pub fn is_syntax_error<'js>(&self, context: &Ctx<'js>, value: &Value<'js>) -> bool {
        let Some(prototype) = &self.syntax_error_prototype else {
            return false;
        };
        value.as_object().is_some_and(|object| {
            object.is_error()
                && prototype
                    .clone()
                    .restore(context)
                    .is_ok_and(|prototype| object.get_prototype().as_ref() == Some(&prototype))
        })
    }

    pub fn error<'js>(
        shared: &Rc<RefCell<Self>>,
        context: &Ctx<'js>,
        format: &Function<'js>,
        error: rquickjs::Error,
    ) -> FeedbackError {
        if !matches!(error, rquickjs::Error::Exception) {
            return FeedbackError::Javascript {
                message: error.to_string(),
                truncated: false,
            };
        }
        Self::thrown(shared, context, format, context.catch())
    }

    pub fn thrown<'js>(
        shared: &Rc<RefCell<Self>>,
        context: &Ctx<'js>,
        format: &Function<'js>,
        value: Value<'js>,
    ) -> FeedbackError {
        let rejections = shared.borrow().rejections.clone();
        if let Some(rejections) = rejections {
            let serialized = rejections
                .restore(context)
                .and_then(|lookup| lookup.call::<_, Option<String>>((value.clone(),)));
            match serialized {
                Ok(Some(serialized)) => match serde_json::from_str(&serialized) {
                    Ok(error) => return error,
                    Err(error) => tracing::debug!(%error, "capability error decoding failed"),
                },
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, "capability error lookup failed"),
            }
        }
        let limit = shared
            .borrow()
            .feedback
            .map_or(96, |limits| limits.error_bytes)
            .min(2048);
        let formatted = format.call::<_, Object>((value, limit)).and_then(|value| {
            let truncated = value.get::<_, bool>("truncated")?;
            let message = value.get::<_, rquickjs::String>("message")?;
            let message = message.to_cstring()?;
            if message.len() > limit * 4 + 32 {
                return Err(rquickjs::Error::Unknown);
            }
            Ok((message.as_str().to_owned(), truncated))
        });
        let (message, truncated) = match formatted {
            Ok(formatted) => formatted,
            Err(error) => {
                tracing::debug!(%error, "exception formatting failed");
                ("JavaScript exception".into(), false)
            }
        };
        FeedbackError::Javascript { message, truncated }
    }

    pub fn bounded(mut error: FeedbackError, limits: FeedbackLimits) -> FeedbackError {
        if crate::encoded_fits(&error, limits.error_bytes) {
            return error;
        }
        let (message, truncated) = match &mut error {
            FeedbackError::Javascript { message, truncated }
            | FeedbackError::Capability {
                message, truncated, ..
            }
            | FeedbackError::Context {
                message, truncated, ..
            }
            | FeedbackError::InvalidOutput { message, truncated } => (message, truncated),
            FeedbackError::OutputLimit { .. } => return error,
        };
        *truncated = true;
        let mut original = std::mem::take(message);
        if !crate::encoded_fits(&error, limits.error_bytes) {
            if !matches!(
                error,
                FeedbackError::Capability { .. } | FeedbackError::Context { .. }
            ) {
                return FeedbackError::OutputLimit {
                    limit_bytes: limits.error_bytes as u32,
                };
            }
            // Tiny error reserves can fit a failure notice but not capability metadata.
            error = FeedbackError::Javascript {
                message: String::new(),
                truncated: true,
            };
            original.insert_str(0, "Host failure; details/effects omitted; ");
        }
        let mut lower = 0;
        let mut upper = original.len();
        while lower < upper {
            let middle = lower + (upper - lower).div_ceil(2);
            let end = original.floor_char_boundary(middle);
            let message = match &mut error {
                FeedbackError::Javascript { message, .. }
                | FeedbackError::Capability { message, .. }
                | FeedbackError::Context { message, .. }
                | FeedbackError::InvalidOutput { message, .. } => message,
                FeedbackError::OutputLimit { .. } => unreachable!(),
            };
            message.clear();
            message.push_str(&original[..end]);
            if crate::encoded_fits(&error, limits.error_bytes) {
                lower = middle;
            } else {
                upper = middle - 1;
            }
        }
        let end = original.floor_char_boundary(lower);
        match &mut error {
            FeedbackError::Javascript { message, .. }
            | FeedbackError::Capability { message, .. }
            | FeedbackError::Context { message, .. }
            | FeedbackError::InvalidOutput { message, .. } => {
                message.clear();
                message.push_str(&original[..end]);
            }
            FeedbackError::OutputLimit { .. } => unreachable!(),
        }
        error
    }
}

#[derive(serde::Serialize)]
struct PrintPreview<'a> {
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_prefix: Option<&'a str>,
}

fn print_preview(
    context: &Ctx<'_>,
    value: Option<&serde_json::Value>,
    limit: usize,
) -> rquickjs::Result<serde_json::Value> {
    let mut preview = PrintPreview {
        truncated: true,
        json_prefix: None,
    };
    let mut buffer = vec![0; limit];
    if let Some(value) = value {
        let mut writer = std::io::Cursor::new(buffer.as_mut_slice());
        if let Err(error) = serde_json::to_writer(&mut writer, value)
            && error.io_error_kind() != Some(std::io::ErrorKind::WriteZero)
        {
            return Err(Exception::throw_type(context, "print encoding failed"));
        }
        let written = writer.position() as usize;
        let prefix = match std::str::from_utf8(&buffer[..written]) {
            Ok(prefix) => prefix,
            Err(error) => std::str::from_utf8(&buffer[..error.valid_up_to()])
                .map_err(|_| Exception::throw_type(context, "print encoding failed"))?,
        };
        preview.json_prefix = Some("");
        if crate::encoded_fits(&preview, limit) {
            let mut lower = 0;
            let mut upper = prefix.len();
            while lower < upper {
                let middle = lower + (upper - lower).div_ceil(2);
                preview.json_prefix = Some(&prefix[..prefix.floor_char_boundary(middle)]);
                if crate::encoded_fits(&preview, limit) {
                    lower = middle;
                } else {
                    upper = middle - 1;
                }
            }
            preview.json_prefix = Some(&prefix[..prefix.floor_char_boundary(lower)]);
        } else {
            preview.json_prefix = None;
        }
    }
    serde_json::to_value(preview)
        .map_err(|_| Exception::throw_type(context, "print encoding failed"))
}

struct Pending {
    method: String,
    result: oneshot::Receiver<Result<serde_json::Value, CapabilityError>>,
    resolve: Persistent<Function<'static>>,
    reject: Persistent<Function<'static>>,
}

enum Encoding<'a> {
    Value(&'a serde_json::Value),
    Array(usize),
    Object(Vec<&'a String>),
}

fn encode<'js>(context: &Ctx<'js>, value: &serde_json::Value) -> rquickjs::Result<Value<'js>> {
    // Own data properties bypass user-defined prototype setters during conversion.
    let mut stack = vec![Encoding::Value(value)];
    let mut values = Vec::new();
    while let Some(encoding) = stack.pop() {
        match encoding {
            Encoding::Value(value) => match value {
                serde_json::Value::Array(array) => {
                    stack.push(Encoding::Array(array.len()));
                    stack.extend(array.iter().rev().map(Encoding::Value));
                }
                serde_json::Value::Object(object) => {
                    stack.push(Encoding::Object(object.keys().collect()));
                    stack.extend(object.values().rev().map(Encoding::Value));
                }
                _ => values.push(
                    rquickjs_serde::to_value(context.clone(), value)
                        .map_err(|_| Exception::throw_type(context, "unsupported host value"))?,
                ),
            },
            Encoding::Array(length) => {
                let children = values.split_off(values.len() - length);
                let array = Array::new(context.clone())?;
                for (index, value) in children.into_iter().enumerate() {
                    array.as_object().prop(
                        index as u32,
                        Property::from(value).writable().enumerable().configurable(),
                    )?;
                }
                values.push(array.into_value());
            }
            Encoding::Object(keys) => {
                let children = values.split_off(values.len() - keys.len());
                let object = Object::new(context.clone())?;
                for (key, value) in keys.into_iter().zip(children) {
                    object.prop(
                        key.as_str(),
                        Property::from(value).writable().enumerable().configurable(),
                    )?;
                }
                values.push(object.into_value());
            }
        }
    }
    values
        .pop()
        .ok_or_else(|| Exception::throw_type(context, "empty host value"))
}

enum Conversion<'js> {
    Value(Value<'js>),
    Array(usize),
    Object(Vec<String>),
}

fn checked_arguments<'js>(
    context: &Ctx<'js>,
    value: Value<'js>,
    limit: usize,
) -> rquickjs::Result<serde_json::Value> {
    let arguments = decode(context, value)?;
    let mut counter = crate::ByteLimit { remaining: limit };
    if serde_json::to_writer(&mut counter, &arguments).is_err() {
        return Err(Exception::throw_type(
            context,
            &format!("argument JSON exceeds {limit} bytes; reduce argument size"),
        ));
    }
    Ok(arguments)
}

fn decode<'js>(context: &Ctx<'js>, value: Value<'js>) -> rquickjs::Result<serde_json::Value> {
    // Serde's nested conversion invokes inherited toJSON hooks before type dispatch.
    let mut stack = vec![Conversion::Value(value)];
    let mut values = Vec::new();
    while let Some(conversion) = stack.pop() {
        match conversion {
            Conversion::Value(value) => {
                if let Some(array) = value.as_array() {
                    stack.push(Conversion::Array(array.len()));
                    for value in array.iter::<Value>().rev() {
                        stack.push(Conversion::Value(value?));
                    }
                } else if let Some(object) = value.as_object() {
                    let fields = object
                        .props::<String, Value>()
                        .collect::<rquickjs::Result<Vec<_>>>()?;
                    let keys = fields.iter().map(|(key, _)| key.clone()).collect();
                    stack.push(Conversion::Object(keys));
                    for (_, value) in fields.into_iter().rev() {
                        stack.push(Conversion::Value(value));
                    }
                } else {
                    values.push(rquickjs_serde::from_value(value).map_err(|_| {
                        Exception::throw_type(context, "unsupported boundary value")
                    })?);
                }
            }
            Conversion::Array(length) => {
                let children = values.split_off(values.len() - length);
                values.push(serde_json::Value::Array(children));
            }
            Conversion::Object(keys) => {
                let children = values.split_off(values.len() - keys.len());
                values.push(serde_json::Value::Object(
                    keys.into_iter().zip(children).collect(),
                ));
            }
        }
    }
    values
        .pop()
        .ok_or_else(|| Exception::throw_type(context, "empty boundary value"))
}
