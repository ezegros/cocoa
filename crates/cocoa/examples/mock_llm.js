((path, headers_json, body_json, identity, session_digest) => {
    const THINKING = 'Fixture arithmetic reasoning.';
    const CELL = 'var invocations = (invocations ?? 0) + 1;\nprint("QuickJS invocation " + invocations);\nprint({result: 7 * 3 + 2, invocation: invocations})';
    const chat = path === '/v1/chat/completions';
    const go = path === '/inference/go/openai/v1/responses';
    const router = path === '/api/v1/responses';
    if (!chat && !go && !router && path !== '/v1/responses') return reject(404);

    let headers, request;
    try {
        headers = JSON.parse(headers_json);
        request = JSON.parse(body_json);
    } catch (error) {
        return reject(400);
    }
    if (!is_object(request)) return reject(400);
    const model = request.model;
    const records = request[chat ? 'messages' : 'input'];
    if (typeof model !== 'string' || model.length === 0 || utf8_prefix(model, 256) !== model
        || !Array.isArray(records) || records.length > 2048 || !records.every(is_object)) {
        return reject(400);
    }

    const authorization = headers.authorization;
    if (chat) {
        if (authorization !== undefined || records.some(record =>
            record.role === 'assistant' && record.reasoning !== THINKING)) return reject(400);
    } else if (typeof authorization !== 'string' || !authorization.startsWith('Bearer ')) {
        return reject(401);
    }

    let call_prefix = 'call_';
    if (go) {
        const session = headers['x-opencode-session'];
        if (typeof headers['user-agent'] !== 'string' || !headers['user-agent'].startsWith('cocoa/')
            || typeof session !== 'string' || !/^[\x21-\x7e]{1,256}$/.test(session)) {
            return reject(400);
        }
        call_prefix += session_digest + '_';
        if (records.some(record => record.type === 'function_call'
            && (typeof record.call_id !== 'string' || !record.call_id.startsWith(call_prefix)))) {
            return reject(400);
        }
    }

    const tools = request.tools === undefined ? [] : request.tools;
    if (!Array.isArray(tools) || tools.length > 32) return reject(400);
    let execution = false;
    for (const tool of tools) {
        if (!is_object(tool)) return reject(400);
        const declaration = chat ? tool.function : tool;
        if (!is_object(declaration)) return reject(400);
        execution ||= declaration.name === 'exec';
    }

    let latest_input = 0;
    for (let index = 0; index < records.length; index++) {
        if (records[index].role === 'user') latest_input = index;
    }
    let feedback;
    for (let index = latest_input; index < records.length; index++) {
        const record = records[index];
        const value = chat
            ? (record.role === 'tool' ? record.content : undefined)
            : (record.type === 'function_call_output' ? record.output : undefined);
        if (typeof value === 'string') feedback = value;
    }

    const events = [];
    if (chat) chat_event({role: 'assistant', reasoning: THINKING});
    let output;
    if (execution && feedback === undefined) {
        const arguments_json = JSON.stringify({code: CELL});
        if (chat) {
            chat_event({tool_calls: [{
                index: 0, id: call_prefix + identity, type: 'function',
                function: {name: 'exec', arguments: arguments_json}
            }]});
            chat_event({}, 'tool_calls');
        } else {
            const call = {
                type: 'function_call', call_id: call_prefix + identity,
                name: 'exec', arguments: arguments_json
            };
            if (router) {
                event({type: 'response.output_item.added', output_index: 0, item: call});
                event({type: 'response.output_item.done', output_index: 0, item: call});
            } else {
                call.id = 'fc_' + identity;
                call.status = 'completed';
            }
            output = [call];
        }
    } else {
        const answer = utf8_prefix(feedback === undefined
            ? 'Mock response from ' + model + '.'
            : 'QuickJS execution feedback:\n' + feedback, 4096);
        const characters = Array.from(answer);
        for (let index = 0; index < characters.length; index += 32) {
            const delta = characters.slice(index, index + 32).join('');
            if (chat) chat_event({content: delta}, null, true);
            else event({type: 'response.output_text.delta', delta}, true);
        }
        if (chat) {
            chat_event({}, 'stop');
        } else {
            const message = {
                id: 'msg_' + identity, type: 'message', role: 'assistant',
                content: [{type: 'output_text', text: answer, annotations: []}]
            };
            if (!router) message.status = 'completed';
            output = [message];
        }
    }
    if (chat) {
        event({
            id: 'chatcmpl_' + identity, object: 'chat.completion.chunk', created: 0, model,
            choices: [], usage: {prompt_tokens: 0, completion_tokens: 0, total_tokens: 0}
        });
        events.push({data: '[DONE]', delayed: false});
    } else {
        event({type: 'response.completed', response: {
            id: 'resp_' + identity, status: 'completed', output,
            usage: {input_tokens: 0, output_tokens: 0}
        }});
    }
    return JSON.stringify({status: 200, events});

    function reject(status) {
        return JSON.stringify({status, events: []});
    }

    function is_object(value) {
        return value !== null && typeof value === 'object' && !Array.isArray(value);
    }

    function utf8_prefix(value, limit) {
        let bytes = 0, end = 0;
        for (const character of value) {
            const point = character.codePointAt(0);
            const size = point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4;
            if (bytes + size > limit) break;
            bytes += size;
            end += character.length;
        }
        return value.slice(0, end);
    }

    function event(value, delayed = false) {
        events.push({data: JSON.stringify(value), delayed});
    }

    function chat_event(delta, finish_reason = null, delayed = false) {
        event({
            id: 'chatcmpl_' + identity, object: 'chat.completion.chunk', created: 0, model,
            choices: [{index: 0, delta, finish_reason}]
        }, delayed);
    }
})
