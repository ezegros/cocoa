((call, convert, limit) => {
    const create = Object.create, define = Object.defineProperty, freeze = Object.freeze;
    const Type = TypeError;
    let namespace = freeze(create(null));
    define(globalThis, 'mcp', {get: () => namespace, enumerable: true});
    return entries => {
        const next = create(null);
        for (let index = 0; index < entries.length; index++) {
            const entry = entries[index], server = entry[0], tool = entry[1], name = entry[2];
            if (!next[server]) define(next, server, {value: create(null), enumerable: true});
            const execute = async input => {
                let value;
                try { value = convert(input, limit()); }
                catch (error) { throw new Type(typeof error === 'string' ? error : 'argument conversion failed'); }
                return call(name, value);
            };
            define(next[server], tool, {value: execute, enumerable: true});
        }
        for (const server in next) freeze(next[server]);
        namespace = freeze(next);
    };
})
