(() => {
    const references = new WeakMap();
    const has = WeakMap.prototype.has;
    const get = WeakMap.prototype.get;
    const set = WeakMap.prototype.set;
    const apply = Reflect.apply;
    const own_keys = Reflect.ownKeys;
    const descriptor = Object.getOwnPropertyDescriptor;
    const create = Object.create;
    const freeze = Object.freeze;
    const prototype = Object.getPrototypeOf;
    const has_own = Object.prototype.hasOwnProperty;
    const is_array = Array.isArray;
    const integer = Number.isInteger;
    const Type = TypeError;
    const contains = value => apply(has, references, [value]);
    const receipt = value => {
        const reference = descriptor(value, 'reference');
        const kind = descriptor(value, 'kind');
        if (reference && typeof reference.value === 'string' &&
            kind && kind.value === 'process') {
            const handle = create(null);
            handle.kind = 'process';
            freeze(handle);
            apply(set, references, [handle, reference.value]);
            return handle;
        }
        const name = descriptor(value, 'name');
        const alias = descriptor(value, 'alias');
        if (!reference || typeof reference.value !== 'string' ||
            !name || typeof name.value !== 'string' ||
            !alias || typeof alias.value !== 'string') {
            throw new Type('invalid spawn receipt');
        }
        const handle = create(null);
        handle.name = name.value;
        handle.alias = alias.value;
        freeze(handle);
        apply(set, references, [handle, reference.value]);
        return handle;
    };
    const wrap = (call, convert, limit, name) => {
        const snapshot = value => {
            try {
                return convert(value, limit());
            } catch (error) {
                throw new Type(typeof error === 'string' ? error : 'argument conversion failed');
            }
        };
        if (name === 'cocoa.spawn' || name === 'cocoa.run') return async input => call(name, snapshot(input));
        if (name === 'cocoa.cancel') return async handle => {
            if (!contains(handle)) throw new Type('expected an agent or process handle');
            const input = create(null);
            input.reference = apply(get, references, [handle]);
            return call(name, snapshot(input));
        };
        return async (input, options = undefined) => {
            const single = contains(input);
            const handles = create(null);
            if (single) handles[''] = apply(get, references, [input]);
            else {
                if (input === null || typeof input !== 'object' || is_array(input)) {
                    throw new Type('expected an agent or process handle or handle group');
                }
                const parent = prototype(input);
                if (parent !== null && prototype(parent) !== null) throw new Type('unsupported handle group');
                const keys = own_keys(input);
                if (keys.length > 4096) throw new Type('handle group node limit exceeded');
                for (let index = 0; index < keys.length; index++) {
                    const key = keys[index];
                    if (typeof key !== 'string') throw new Type('symbol key in handle group');
                    const field = descriptor(input, key);
                    if (!field || !apply(has_own, field, ['value']) || !contains(field.value)) {
                        throw new Type('expected an agent or process handle data property');
                    }
                    handles[key] = apply(get, references, [field.value]);
                }
            }
            const arguments_value = create(null);
            arguments_value.handles = handles;
            if (options !== undefined) {
                const selected = snapshot(options);
                if (selected === null || typeof selected !== 'object' || is_array(selected)) {
                    throw new Type('expected wait options');
                }
                const keys = own_keys(selected);
                for (let index = 0; index < keys.length; index++) {
                    if (keys[index] !== 'timeout_ms') throw new Type('unknown wait option');
                    const timeout = selected.timeout_ms;
                    if (!integer(timeout) || timeout < 0 || timeout > 4294967295) {
                        throw new Type('timeout_ms must be a u32');
                    }
                    arguments_value.timeout_ms = timeout === 0 ? 0 : timeout;
                }
            }
            const result = await call(name, snapshot(arguments_value));
            if (!single) return result;
            const outcome = descriptor(result, '');
            if (!outcome) throw new Type('missing single wait outcome');
            return outcome.value;
        };
    };
    return {contains, receipt, wrap};
})()
