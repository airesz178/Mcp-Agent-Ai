/** The model provider is replaceable; it never decides filesystem permissions. */
const PRESETS = Object.freeze({
  openai: { baseURL: 'https://api.openai.com/v1', key: 'OPENAI_API_KEY' },
  openrouter: { baseURL: 'https://openrouter.ai/api/v1', key: 'OPENROUTER_API_KEY' },
  anthropic: { baseURL: 'https://api.anthropic.com/v1', key: 'ANTHROPIC_API_KEY' },
  ollama: { baseURL: 'http://127.0.0.1:11434/v1', key: null },
});

export function providerNames() { return [...Object.keys(PRESETS), 'hermes']; }

function endpoint(url, suffix) {
  const parsed = new URL(url);
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(parsed.hostname);
  if ((parsed.protocol !== 'https:' && !(parsed.protocol === 'http:' && local)) ||
      parsed.username || parsed.password || parsed.search || parsed.hash) {
    throw new Error('Provider endpoint requires HTTPS (or HTTP loopback) without credentials in URL');
  }
  return `${parsed.href.replace(/\/$/, '')}/${suffix}`;
}

async function fetchJson(url, body, headers, fetchFn) {
  let response;
  try {
    response = await fetchFn(url, {
      method: 'POST', headers: { 'content-type': 'application/json', ...headers },
      body: JSON.stringify(body), redirect: 'error', signal: AbortSignal.timeout(45_000),
    });
  } catch (error) {
    throw new Error(`Provider request failed: ${error.message}`);
  }
  // Provider error bodies can contain prompts, echoed tokens or arbitrary HTML.
  if (!response.ok) throw new Error(`Provider HTTP ${response.status}`);
  const contentLength = Number(response.headers.get('content-length') ?? 0);
  if (contentLength > 2 * 1024 * 1024) throw new Error('Provider response too large');
  if (!response.body) throw new Error('Provider returned no response body');
  const reader = response.body.getReader();
  const chunks = [];
  let totalBytes = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      totalBytes += value.byteLength;
      if (totalBytes > 2 * 1024 * 1024) {
        await reader.cancel().catch(() => {});
        throw new Error('Provider response too large');
      }
      chunks.push(Buffer.from(value));
    }
  } finally { reader.releaseLock(); }
  return JSON.parse(Buffer.concat(chunks, totalBytes).toString('utf8'));
}

function toOpenAIMessages(messages) {
  return messages.map(message => {
    if (message.role === 'assistant' && message.toolCalls?.length) {
      return { role: 'assistant', content: message.text || null, tool_calls: message.toolCalls.map(call => ({
        id: call.id, type: 'function', function: { name: call.name, arguments: call.arguments },
      })) };
    }
    if (message.role === 'tool') return { role: 'tool', tool_call_id: message.callId, content: message.text };
    return { role: message.role, content: message.text };
  });
}

function toAnthropicMessages(messages) {
  const result = [];
  for (const item of messages.filter(message => message.role !== 'system')) {
    if (item.role === 'assistant' && item.toolCalls?.length) {
      result.push({ role: 'assistant', content: [
        ...(item.text ? [{ type: 'text', text: item.text }] : []),
        ...item.toolCalls.map(call => ({ type: 'tool_use', id: call.id, name: call.name, input: JSON.parse(call.arguments) })),
      ] });
    } else if (item.role === 'tool') {
      const previous = result.at(-1);
      const block = { type: 'tool_result', tool_use_id: item.callId, content: item.text };
      if (previous?.role === 'user' && Array.isArray(previous.content) && previous.content[0]?.type === 'tool_result') {
        previous.content.push(block);
      } else result.push({ role: 'user', content: [block] });
    } else result.push({ role: item.role, content: item.text });
  }
  return result;
}

export function makeProvider(name, { env = process.env, fetchFn = fetch } = {}) {
  const preset = PRESETS[name];
  if (!preset) throw new Error(`Unknown provider: ${name}`);
  const apiKey = preset.key ? env[preset.key] : undefined;
  if (preset.key && !apiKey) throw new Error(`Set ${preset.key} in the environment before using ${name}`);
  const baseURL = env.COS_FUSION_ENDPOINT || preset.baseURL;

  if (name === 'anthropic') {
    const url = endpoint(baseURL, 'messages');
    return {
      id: name, async generate({ model, messages, tools }) {
        const response = await fetchJson(url, {
          model, max_tokens: 2048,
          system: messages.filter(message => message.role === 'system').map(message => message.text).join('\n\n'),
          messages: toAnthropicMessages(messages),
          tools: tools.map(tool => ({ name: tool.name, description: tool.description, input_schema: tool.parameters })),
        }, { 'x-api-key': apiKey, 'anthropic-version': '2023-06-01' }, fetchFn);
        if (!Array.isArray(response.content)) throw new Error('Invalid Anthropic response');
        return {
          text: response.content.filter(block => block.type === 'text').map(block => block.text).join('\n'),
          toolCalls: response.content.filter(block => block.type === 'tool_use').map(block => ({
            id: block.id, name: block.name, arguments: JSON.stringify(block.input ?? {}),
          })),
        };
      },
    };
  }

  const url = endpoint(baseURL, 'chat/completions');
  return {
    id: name, async generate({ model, messages, tools }) {
      const response = await fetchJson(url, {
        model, messages: toOpenAIMessages(messages),
        ...(tools.length ? { tools: tools.map(tool => ({ type: 'function', function: {
          name: tool.name, description: tool.description, parameters: tool.parameters,
        } })) } : {}),
      }, apiKey ? { authorization: `Bearer ${apiKey}` } : {}, fetchFn);
      const message = response.choices?.[0]?.message;
      if (!message) throw new Error('Invalid OpenAI-compatible response');
      return {
        text: typeof message.content === 'string' ? message.content : '',
        toolCalls: (message.tool_calls ?? []).map(call => ({
          id: call.id, name: call.function.name, arguments: call.function.arguments,
        })),
      };
    },
  };
}
