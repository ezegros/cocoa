# Providers

Select an alias or discovered model with `/models`, or use `cocoa --model provider/model` for a direct selection. Aliases can contain choices from different providers; see [model aliases](configuration.md#model-aliases). Use a tool-capable model; image and thinking support depend on the provider and model. Where discovery is unavailable, enter the model ID explicitly. Saving a credential does not verify inference access or account eligibility.

## Supported providers

Run `cocoa login PROVIDER` for hidden key entry, or set the environment variable below. ChatGPT uses browser sign-in; Ollama runs locally. See [credential precedence and storage](configuration.md#credentials).

| Provider | Default credential | Service or billing scope |
| --- | --- | --- |
| `openai` | `OPENAI_API_KEY` | OpenAI Platform API |
| `chatgpt` | Browser sign-in | Eligible ChatGPT plan; see below |
| `anthropic` | `ANTHROPIC_API_KEY` | Anthropic Messages API, not a Claude subscription |
| `ollama` | None | Local Ollama at `127.0.0.1:11434` |
| `opencode-go` | `OPENCODE_API_KEY` | Go-enabled Console key |
| `opencode-zen` | `OPENCODE_API_KEY` | Zen GPT Responses models |
| `baseten` | `BASETEN_API_KEY` | Hosted Model APIs, not arbitrary deployments |
| `openrouter` | `OPENROUTER_API_KEY` | OpenRouter API |
| `xai` | `XAI_API_KEY` | API billing, not a Grok subscription |
| `vercel-ai-gateway` | `AI_GATEWAY_API_KEY` | Vercel AI Gateway |
| `together` | `TOGETHER_API_KEY` | Serverless GLM-5.2 and GLM-5.3 Responses models |
| `fireworks` | `FIREWORKS_API_KEY` | Responses API |
| `kimi-code` | `KIMI_API_KEY` | Kimi Code subscription API key |
| `meta` | `META_API_KEY` | Model API, not Muse Code subscription credentials |
| `groq` | `GROQ_API_KEY` | Responses API |
| `deepseek` | `DEEPSEEK_API_KEY` | DeepSeek API |
| `cerebras` | `CEREBRAS_API_KEY` | Cerebras inference |
| `nvidia` | `NVIDIA_API_KEY` | NVIDIA NIM |
| `huggingface` | `HF_TOKEN` | Hugging Face inference router |
| `ant-ling` | `ANT_LING_API_KEY` | Ant Ling API |
| `moonshotai` | `MOONSHOT_API_KEY` | Moonshot Platform, international |
| `moonshotai-cn` | `MOONSHOT_API_KEY` | Moonshot Platform, China |
| `zai` | `ZAI_API_KEY` | Z.AI Coding API |
| `zai-coding-cn` | `ZAI_CODING_CN_API_KEY` | Z.AI Coding API, China |
| `qwen-token-plan` | `QWEN_TOKEN_PLAN_API_KEY` | Qwen token plan, Singapore |
| `qwen-token-plan-cn` | `QWEN_TOKEN_PLAN_CN_API_KEY` | Qwen token plan, Beijing |
| `qwen-token-plan-individual` | `QWEN_TOKEN_PLAN_API_KEY` | Qwen individual token plan, Singapore |
| `qwen-token-plan-individual-cn` | `QWEN_TOKEN_PLAN_CN_API_KEY` | Qwen individual token plan, Beijing |
| `xiaomi` | `XIAOMI_API_KEY` | MiMo API |
| `xiaomi-token-plan-cn` | `XIAOMI_TOKEN_PLAN_CN_API_KEY` | MiMo token plan, China |
| `xiaomi-token-plan-ams` | `XIAOMI_TOKEN_PLAN_AMS_API_KEY` | MiMo token plan, Amsterdam |
| `xiaomi-token-plan-sgp` | `XIAOMI_TOKEN_PLAN_SGP_API_KEY` | MiMo token plan, Singapore |

Model access depends on your account and the provider's supported models. A custom endpoint must support the selected provider's API; an "OpenAI-compatible" label alone does not establish compatibility. See [endpoint configuration](configuration.md#credentials).

## Local Ollama

Start Ollama and install a tool-capable model before launching Cocoa. For a model installed as `qwen3-coder`:

```sh
cocoa --model ollama/qwen3-coder
```

No credential is required. Choose a model and context size that fit your available memory.

## ChatGPT sign-in

Run `cocoa login chatgpt` or `/login chatgpt`, confirm plan use and finish browser sign-in. Choose a model from the active account's catalog. `/usage` opens [Manage usage](https://chatgpt.com/settings/usage).

The account menu can add, reconnect, select and sign out accounts. Identity-only sign-in does not enable inference. Restart Cocoa after switching accounts; existing sessions may require their original account to resume.

ChatGPT and `openai` are separate billing products. Cocoa does not add Platform API fallback automatically. If you include an `openai` choice with `fallback_only: true` in the same alias, an eligible [startup failure](configuration.md#startup-fallback) can use it and incur API charges. ChatGPT rejects API-key and custom-endpoint overrides. [Compaction](configuration.md#compaction) uses the same ChatGPT account without switching to the Platform API.

Paid or remotely hosted distribution of this ChatGPT integration requires OpenAI approval. Check the [integration terms](https://developers.openai.com/cookbook/articles/sign-in-with-chatgpt) before redistributing.

## Thinking and model restrictions

Omit `effort` in an alias choice to use the provider default. The `/models` alias picker selects saved regular choices without changing their saved effort. Use `/aliases` or `cocoa aliases` to edit that effort. For a direct selection, use `--thinking LEVEL` or select a level on the separate provider model browser's effort scale; `default` leaves it to the provider. Cocoa rejects levels unsupported by the selected provider and model.

- `kimi-code` accepts `k3.1-pro` and `k3.1-pro-highspeed`. Use `max`, not `xhigh`, for maximum effort.
- `deepseek` supports thinking for `deepseek-flash` and `deepseek-pro`, including explicit `none`.
- Moonshot accepts `low`, `high` and `max` for its supported Kimi models.
- Anthropic supports thinking levels for selected adaptive-thinking models, not older token-budget settings.

API support does not authorize another product's subscription credentials. Cocoa has no Claude.ai, Copilot, Gemini CLI, Antigravity or Grok subscription login. Anthropic API keys remain separate from [Claude plan credentials](https://code.claude.com/docs/en/legal-and-compliance), and Meta API keys remain separate from [Muse Code subscriptions](https://dev.meta.ai/docs/muse-code/subscriptions).
