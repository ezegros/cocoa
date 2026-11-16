((call, convert, limit, name) => {
    const Type = TypeError;
    const create = Object.create, is_array = Array.isArray;
    const snapshot = input => {
        try {
            return convert(input, limit());
        } catch (error) {
            throw new Type(typeof error === 'string' ? error : 'argument conversion failed');
        }
    };
    if (name === 'context.get' || name === 'context.set') return async (...values) => {
        const count = name === 'context.get' ? 1 : 2;
        if (values.length !== count) throw new Type(name + ' requires exactly ' + count + ' arguments');
        const input = create(null);
        input.key = values[0];
        if (count === 2) input.content = values[1];
        return call(name, snapshot(input));
    };
    if (name === 'context.list') return async (...values) => {
        if (values.length !== 1) throw new Type(name + ' requires exactly 1 argument');
        const input = snapshot(values[0]);
        if (input === null || typeof input !== 'object' || is_array(input)) throw new Type('context.list requires a filters object');
        return call(name, input);
    };
    return async input => call(name, snapshot(input));
})
