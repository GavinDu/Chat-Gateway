# Provider terms and opt-in policy

**Reviewed links:** 2026-10-25  
**Scope:** consumer web interfaces driven by browser automation in this repository.  
**This is an engineering risk register, not legal advice or a legal determination.**
Terms differ by region, product, account type, subscription, and feature. Re-check
the terms that apply to your account before enabling a provider, and record the
actual review owner/date below.

## Default posture

- The shipped `config.example.yaml` enables **Manus** only per an explicit
  owner request; the other eight providers remain disabled. This is a
  configuration example, **not** a determination that browser automation is
  authorized by any provider's terms. For your own account, enable or retain
  a provider only after reviewing its current terms, usage policy, and whether
  the intended browser automation is permitted.
- Existing local `config.yaml` files are not overwritten by `docker-run.sh init`.
  Changing an example or opening the official website does not link a session
  or prove authenticated Chat/Agent execution.
- Keep this gateway for personal, single-user use unless you have explicit rights
  and authorization for another deployment model. Do not share accounts or expose
  the gateway as a service to other users.
- Browser automation can be prohibited even when manual use of a consumer chat
  site is allowed. Do not assume that a paid subscription authorizes scraping,
  automated extraction, scripted UI access, account sharing, or resale.
- Providers may restrict access, rate-limit, challenge, suspend, or terminate
  accounts. A successful technical probe does not establish policy compliance.
- Prefer official APIs under their API terms where available. Do not bypass
  CAPTCHAs, access controls, rate limits, or anti-bot measures.
- Never use production credentials or sensitive prompts in a provider canary.
  Store session files as secrets; they may be equivalent to passwords.

## Official terms and policy links

The links below were checked on **2026-10-25** for reachability and provider
identity. This is not a clause-by-clause legal review. Confirm the terms shown
for your country and account before enabling a provider.

| Provider | Official terms / policies | Review note |
| --- | --- | --- |
| Qwen | [Qwen Terms of Service](https://qwen.ai/termsservice) | The reviewed terms expressly prohibit automated interaction/extraction from Qwen services. Do **not** enable this browser adapter unless the provider gives written permission or the applicable terms change. |
| Meta AI | [Meta Terms of Service](https://www.facebook.com/legal/terms/) · [Meta AI product page](https://ai.meta.com/meta-ai/assistant/) | The product page is not itself authorization for automation. Confirm which Meta terms and supplemental policies apply to the account and region. |
| Claude | [Anthropic Legal Center](https://www.anthropic.com/legal) | Select the consumer terms applicable to Claude Free/Pro/Max, or the relevant commercial terms; do not infer API terms apply to browser UI use. |
| ChatGPT | [OpenAI Terms of Use](https://openai.com/policies/terms-of-use/) · [Service Terms](https://openai.com/policies/service-terms/) | The reviewed individual-use terms prohibit automatic/programmatic extraction of data or output. Do **not** enable this browser adapter unless the applicable terms or written permission authorize the intended automation. |
| Gemini | [Google Terms of Service](https://www.about.google/policies/terms/) · [Service-specific terms](https://policies.google.com/terms/service-specific?hl=en) · [Generative AI Prohibited Use Policy](https://policies.google.com/terms/generative-ai/use-policy?hl=en-US) | Check the Gemini Apps terms specifically; API terms are not interchangeable with consumer web terms. |
| Grok | [Consumer Terms of Service](https://x.ai/legal/terms-of-service) · [SpaceXAI Legal Center](https://x.ai/legal) | Confirm whether the terms for Grok on x.com or the standalone Grok service apply to the account. |
| Mistral | [Consumer Terms outside the EEA](https://legal.mistral.ai/terms/row-consumer-terms/) · [Legal Center](https://legal.mistral.ai/terms/get-started/) | The reviewed consumer terms restrict extracting content except through permitted product functionality. Obtain clarification before enabling scripted UI extraction. |
| Perplexity | [Terms of Service](https://www.perplexity.ai/hub/legal/terms-of-service) | Confirm consumer terms and any service-specific terms for the subscribed plan; API terms are separate. |
| Manus | [Terms of Service](https://manus.im/terms) | The owner-requested example has `enabled: true`, **not blanket permission to automate**. Review the terms and account authorization before running it; disable Manus in your own config if permission cannot be established. Chat Mode and Agent Mode have different execution semantics. Session linking, selector reliability and task completion still require signed-in validation. |

## Review record

For each provider you intend to enable, record:

- reviewer and date;
- the exact terms and usage-policy versions reviewed;
- whether automated browser interaction and output retrieval are permitted;
- any required subscription, rate limit, or personal-use restriction;
- whether written permission or a provider-approved API is required.

If this cannot be established, leave the provider disabled. Re-review links periodically
and after provider terms or adapter behavior changes.
