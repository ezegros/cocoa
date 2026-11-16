((is_handle) => {
    const own_keys = Reflect.ownKeys;
    const descriptor = Object.getOwnPropertyDescriptor;
    const create = Object.create;
    const set_prototype = Object.setPrototypeOf;
    const is_array = Array.isArray;
    const finite = Number.isFinite;
    const apply = Reflect.apply;
    const prototype = Object.getPrototypeOf;
    const has_own = Object.prototype.hasOwnProperty;
    const SetType = Set;
    const has = Set.prototype.has;
    const add = Set.prototype.add;
    const remove = Set.prototype.delete;
    const stringify = JSON.stringify;
    const slice = String.prototype.slice;
    const character_code = String.prototype.charCodeAt;
    const identifier = /^[A-Za-z_$][A-Za-z0-9_$]*$/;
    const execute = RegExp.prototype.exec;
    const frame = (value, parent, key, depth, exit, path) => {
        const item = create(null);
        item.value = value;
        item.parent = parent;
        item.key = key;
        item.depth = depth;
        item.exit = exit;
        item.path = path;
        return item;
    };
    return (value, bytes, selected = false, overflow = undefined) => {
        const maximum = bytes;
        const check = path => {
            if (bytes < 0) throw selected ? overflow : 'argument JSON exceeds ' + maximum + ' bytes at ' + path + '; reduce argument size';
        };
        const string_bytes = (text, path) => {
            bytes -= 2;
            for (let index = 0; index < text.length; index++) {
                const code = apply(character_code, text, [index]);
                if (code === 34 || code === 92 || code === 8 || code === 9 || code === 10 || code === 12 || code === 13) bytes -= 2;
                else if (code < 32) bytes -= 6;
                else if (code < 128) bytes--;
                else if (code < 2048) bytes -= 2;
                else {
                    const next = apply(character_code, text, [index + 1]);
                    if (code >= 0xd800 && code <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) {
                        bytes -= 4;
                        index++;
                    } else bytes -= 3;
                }
                check(path);
            }
        };
        const root = create(null);
        const stack = [frame(value, root, 'value', 0, false, '$')];
        set_prototype(stack, null);
        const ancestors = new SetType();
        let nodes = 0;
        while (stack.length) {
            const item = stack[stack.length - 1];
            stack.length--;
            const value = item.value;
            if (item.exit) {
                apply(remove, ancestors, [value]);
                continue;
            }
            if (++nodes > 4096 || item.depth > 32) {
                throw 'conversion depth/node limit at ' + item.path;
            }
            const type = typeof value;
            if (value === null || type === 'boolean' || type === 'number' || type === 'string') {
                if (type === 'number' && !finite(value)) throw 'nonfinite number at ' + item.path;
                if (selected) bytes -= type === 'string' ? value.length + 2 : 32;
                else if (type === 'string') string_bytes(value, item.path);
                else if (value === null) bytes -= 4;
                else if (type === 'boolean') bytes -= value ? 4 : 5;
                // Rust checks the final encoding, whose number spelling can differ from JavaScript.
                else bytes--;
                item.parent[item.key] = value;
            } else if (type === 'object') {
                if (is_handle(value)) throw 'opaque work handle at ' + item.path;
                if (apply(has, ancestors, [value])) throw 'cyclic value at ' + item.path;
                apply(add, ancestors, [value]);
                const keys = own_keys(value);
                if (keys.length > 4096 - nodes) throw 'conversion node limit at ' + item.path;
                const array = is_array(value);
                const parent = prototype(value);
                if (!array && parent !== null && prototype(parent) !== null) {
                    throw 'unsupported object at ' + item.path;
                }
                const copy = array ? [] : create(null);
                if (array) set_prototype(copy, null);
                item.parent[item.key] = copy;
                stack[stack.length] = frame(value, null, '', 0, true, item.path);
                let length = 0;
                if (array) {
                    length = descriptor(value, 'length').value;
                    if (length > 4096 || keys.length !== length + 1) throw 'sparse or decorated array at ' + item.path;
                    copy.length = length;
                }
                bytes -= 2;
                const count = array ? length : keys.length;
                if (!selected && count) bytes -= count - 1;
                for (let index = 0; index < keys.length; index++) {
                    const key = keys[index];
                    if (typeof key !== 'string') throw 'symbol key at ' + item.path;
                    if (array && key === 'length') continue;
                    const name = apply(slice, key, [0, 80]);
                    const path = apply(slice, item.path + (array ? '[' + name + ']' : apply(execute, identifier, [name]) !== null ? '.' + name : '[' + stringify(name) + ']'), [0, 512]);
                    const field = descriptor(value, key);
                    if (!field || !apply(has_own, field, ['value'])) throw 'accessor at ' + path;
                    if (!array && !field.enumerable) throw 'nonenumerable property at ' + path;
                    if (array && (key >>> 0) + '' !== key) throw 'decorated array at ' + path;
                    if (selected) bytes -= key.length + 4;
                    else if (!array) {
                        string_bytes(key, path);
                        bytes--;
                    }
                    stack[stack.length] = frame(field.value, copy, key, item.depth + 1, false, path);
                }
            } else {
                throw type + ' at ' + item.path;
            }
            check(item.path);
        }
        return root.value;
    };
})
