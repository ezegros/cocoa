//! Worker-owned QuickJS lifecycle, promise settlement and cooperative interruption.

use std::{cell::RefCell, rc::Rc, thread, time::Duration};

use cocoa_core::execution::{CellResponse, ExecutionError, FeedbackError};
use rquickjs::{Context, Exception, Function, Object, Persistent, Runtime, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{CellCommand, Command, QuickJsError, QuickJsOptions, bindings::Bindings};

pub(crate) fn worker(
    options: QuickJsOptions,
    mut commands: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<(), QuickJsError>>,
    stop: CancellationToken,
) {
    let Interpreter {
        mut runtime,
        mut context,
    } = match interpreter(&options) {
        Ok(interpreter) => interpreter,
        Err(error) => {
            if ready
                .send(Err(QuickJsError::Initialization {
                    message: error.to_string(),
                }))
                .is_err()
            {
                tracing::debug!("QuickJS constructor was dropped");
            }
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    let mut format = None;
    let bindings = Rc::new(RefCell::new(Bindings::default()));
    while let Some(command) = commands.blocking_recv() {
        if stop.is_cancelled() {
            break;
        }
        let command = match command {
            Command::Execute(command) => command,
            Command::Cancel { reply } => {
                let result = reset(&mut runtime, &mut context, &mut format, &bindings, &options);
                let failed = result.is_err();
                if reply.send(result).is_err() {
                    tracing::debug!("cancellation caller was dropped");
                }
                if failed {
                    break;
                }
                continue;
            }
        };
        let cancellation = command.cancellation.clone();
        let interrupt_stop = stop.clone();
        let deadline = command.limits.deadline.into_std();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            cancellation.is_cancelled()
                || interrupt_stop.is_cancelled()
                || std::time::Instant::now() >= deadline
        })));
        let rejections = Rc::new(RefCell::new(Rejections::default()));
        let tracker = rejections.clone();
        runtime.set_host_promise_rejection_tracker(Some(Box::new(
            move |context, promise, value, handled| {
                let mut tracker = tracker.borrow_mut();
                let promise = Persistent::save(&context, promise);
                if handled {
                    tracker.pending.retain(|(pending, _)| pending != &promise);
                } else if tracker.pending.len() < 256 {
                    tracker
                        .pending
                        .push((promise, Persistent::save(&context, value)));
                } else {
                    tracker.exceeded = true;
                }
            },
        )));
        bindings.borrow_mut().output_bytes = command.limits.output_bytes.min(256 * 1024);
        bindings.borrow_mut().feedback = Some(command.feedback);
        bindings.borrow_mut().observations = Some(command.prints.clone());
        bindings.borrow_mut().host = Some(command.host.clone());
        let result = evaluate(
            &context,
            &mut format,
            &bindings,
            &command,
            &stop,
            &rejections,
        );
        runtime.set_host_promise_rejection_tracker(None);
        let printed = std::mem::take(&mut bindings.borrow_mut().printed);
        let interrupted = command.cancellation.is_cancelled() || stop.is_cancelled();
        let result = if interrupted {
            Ok(CellResponse::Interrupted { printed })
        } else {
            let outcome = match bindings.borrow().with_host_failures(result) {
                Ok(()) => CellResponse::Completed { printed },
                Err(error) => CellResponse::Failed {
                    error: Bindings::bounded(error, command.feedback),
                    printed,
                },
            };
            if crate::encoded_fits(&outcome, command.feedback.output_bytes) {
                Ok(outcome)
            } else {
                Err(cocoa_core::capability::CapabilityError::OutputLimit.into())
            }
        };
        bindings.borrow_mut().reset();
        drop(rejections);
        runtime.set_interrupt_handler(None);
        let expired = std::time::Instant::now() >= deadline;
        let result = if !interrupted && expired {
            Err(cocoa_core::capability::CapabilityError::Deadline.into())
        } else {
            result
        };
        let reset = if interrupted || expired {
            reset(&mut runtime, &mut context, &mut format, &bindings, &options)
        } else {
            Ok(())
        };
        let failed = reset.is_err();
        if command.reply.send(reset.and(result)).is_err() {
            tracing::debug!("cell caller was dropped");
        }
        if failed {
            break;
        }
    }
    runtime.set_interrupt_handler(None);
    // Clearing persistent helpers breaks the cycle through realm callbacks to bindings.
    *bindings.borrow_mut() = Bindings::default();
}

#[derive(Default)]
struct Rejections {
    pending: Vec<(Persistent<Value<'static>>, Persistent<Value<'static>>)>,
    exceeded: bool,
}

fn evaluate(
    context: &Context,
    format: &mut Option<Persistent<Function<'static>>>,
    bindings: &Rc<RefCell<Bindings>>,
    command: &CellCommand,
    stop: &CancellationToken,
    rejections: &RefCell<Rejections>,
) -> Result<(), FeedbackError> {
    let check_interruption = || {
        if command.cancellation.is_cancelled()
            || stop.is_cancelled()
            || std::time::Instant::now() >= command.limits.deadline.into_std()
        {
            Err(FeedbackError::Javascript {
                message: "JavaScript interrupted".into(),
                truncated: false,
            })
        } else {
            Ok(())
        }
    };
    context.with(|context| {
        let setup = (|| {
            let formatter = match format {
                Some(format) => format.clone().restore(&context)?,
                None => {
                    let formatter = Bindings::install(bindings, &context, &command.methods)?;
                    *format = Some(Persistent::save(&context, formatter.clone()));
                    formatter
                }
            };
            Bindings::refresh(bindings, &context, &command.bindings)?;
            Ok::<_, rquickjs::Error>((formatter, context.eval_promise(command.cell.code.as_str())))
        })();
        check_interruption()?;
        let (format, setup) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                return Err(FeedbackError::Javascript {
                    message: format!("cell initialization failed: {error}"),
                    truncated: false,
                });
            }
        };
        let mut promise = None;
        let mut result = None;
        match setup {
            Ok(value) => promise = Some(value),
            Err(rquickjs::Error::Exception) => {
                let value = context.catch();
                // Async global evaluation rejects runtime throws through its promise.
                // Synchronous failures can also be resource errors, not just parsing.
                let parsing_failed = bindings.borrow().is_syntax_error(&context, &value);
                let stack = value
                    .as_object()
                    .cloned()
                    .and_then(Exception::from_object)
                    .and_then(|error| error.stack());
                let location = stack.as_deref().and_then(|stack| {
                    stack.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix("at eval_script:")?
                            .split(':')
                            .next()?
                            .parse::<u32>()
                            .ok()
                    })
                });
                let mut error = Bindings::thrown(bindings, &context, &format, value);
                if parsing_failed
                    && let FeedbackError::Javascript { message, .. } = &mut error
                {
                    message.insert_str(0, "Cell not executed: JavaScript parsing failed. Declarations, assignments and host operations in this cell did not run. Correct and resubmit the complete step. ");
                }
                if let Some(line) = location.filter(|line| *line > 0)
                    && let Some(source) = command.cell.code.lines().nth(line as usize - 1)
                    && let FeedbackError::Javascript { message, .. } = &mut error
                {
                    let excerpt: String = source.chars().take(120).collect();
                    message.push_str(&format!(" at cell line {line}: {excerpt}"));
                }
                result = Some(Err(error));
            }
            Err(error) => result = Some(Err(Bindings::error(bindings, &context, &format, error))),
        }
        let mut completion = true;
        loop {
            check_interruption()?;
            let polled = Bindings::poll(bindings, &context);
            check_interruption()?;
            if let Err(error) = polled {
                result = Some(Err(Bindings::error(bindings, &context, &format, error)));
                promise = None;
            }
            if let Some(error) = bindings.borrow_mut().failure.take() {
                result = Some(Err(error));
                promise = None;
            }
            if let Some(current) = &promise
                && let Some(value) = current.result::<Value>()
            {
                // QuickJS async global eval boxes completion instead of assimilating it.
                let value = if completion {
                    completion = false;
                    value.and_then(|value| Object::from_value(value)?.get::<_, Value>("value"))
                } else {
                    value
                };
                check_interruption()?;
                match value {
                    Ok(value) => {
                        promise = value.as_promise().cloned();
                        if promise.is_none() {
                            result = Some(Ok(()));
                        }
                    }
                    Err(error) => {
                        result = Some(Err(Bindings::error(bindings, &context, &format, error)));
                        promise = None;
                    }
                }
            }
            let executed = context.execute_pending_job();
            check_interruption()?;
            if !executed {
                if !bindings.borrow().is_pending()
                    && let Some(settled) = result.take()
                {
                    let rejection = rejections
                        .borrow()
                        .pending
                        .first()
                        .map(|(_, value)| value.clone());
                    if rejections.borrow().exceeded {
                        return Err(FeedbackError::Javascript {
                            message: "unhandled rejection limit exceeded".into(),
                            truncated: false,
                        });
                    }
                    if settled.is_ok()
                        && let Some(rejection) = rejection
                    {
                        result = Some(match rejection.restore(&context) {
                            Ok(value) => Err(Bindings::thrown(bindings, &context, &format, value)),
                            Err(error) => Err(Bindings::error(bindings, &context, &format, error)),
                        });
                        continue;
                    }
                    return match bindings.borrow_mut().failure.take() {
                        Some(error) => Err(error),
                        None => settled,
                    };
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    })
}

struct Interpreter {
    context: Context,
    runtime: Runtime,
}

fn interpreter(options: &QuickJsOptions) -> Result<Interpreter, ExecutionError> {
    let runtime = Runtime::new().map_err(|error| crate::unavailable(error.to_string()))?;
    runtime.set_memory_limit(options.memory_bytes);
    runtime.set_max_stack_size(options.stack_bytes);
    let context = Context::full(&runtime).map_err(|error| crate::unavailable(error.to_string()))?;
    Ok(Interpreter { context, runtime })
}

fn reset(
    runtime: &mut Runtime,
    context: &mut Context,
    format: &mut Option<Persistent<Function<'static>>>,
    bindings: &Rc<RefCell<Bindings>>,
    options: &QuickJsOptions,
) -> Result<(), ExecutionError> {
    // Pending jobs belong to the runtime, not just the context being discarded.
    let replacement = interpreter(options)?;
    runtime.set_interrupt_handler(None);
    *bindings.borrow_mut() = Bindings::default();
    *format = None;
    *context = replacement.context;
    *runtime = replacement.runtime;
    Ok(())
}
