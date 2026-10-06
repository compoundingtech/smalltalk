// Registers only a loopback provider. All ask UI, controls, transport, and results are native.
export default function (pi) {
  pi.registerProvider('control-smoke', {
    baseUrl: process.env.SMOKE_BASE_URL,
    apiKey: 'isolated-zero-cost-provider', api: 'openai-completions',
    models: [{ id: 'native-smoke', name: 'Native ask smoke', reasoning: false, input: ['text'], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 32000, maxTokens: 2000 }],
  });
  // Tool events are written outside the TUI so stdout remains a real terminal surface.
  pi.on('tool_result', async event => {
    if (event.toolName !== 'ask') return;
    await Bun.write(process.env.SMOKE_ROOT + '/native-result-' + event.toolCallId + '.json', JSON.stringify({ tool_call_id: event.toolCallId, is_error: event.isError, details: event.details, content: event.content }));
  });
}
