# Где взять ключ

Ключ — это пароль для доступа к модели. Текст платный: деньги списываются с твоего аккаунта у
провайдера (компании, которая продаёт доступ к моделям), поэтому ключ держи при себе.

## OpenRouter

`base_url` — `https://openrouter.ai/api/v1` · модель — `deepseek/deepseek-v4.1-flash-1017`.
Один аккаунт — сотни моделей. Регистрация на [openrouter.ai](https://openrouter.ai) (почта,
Google или GitHub), затем [Keys](https://openrouter.ai/keys): **Create Key**, имя («snens»), по
желанию лимит расходов. Ключ начинается с `sk-or-` и виден один раз. Бесплатные модели (с `:free`
на конце имени) — без карты и с суточным лимитом, платные — после пополнения на
[Credits](https://openrouter.ai/settings/credits).

## DeepSeek

`base_url` — `https://api.deepseek.com` · модель — `deepseek-flash` (умнее и дороже —
`deepseek-v4-pro`). Напрямую, без посредника, дёшево. Регистрация на
[platform.deepseek.com](https://platform.deepseek.com), затем
[API keys](https://platform.deepseek.com/api_keys): создать и скопировать сразу — второй раз
не покажут. Стартового лимита документация не обещает: нужен баланс, пополнение — **Top Up**
(карта, PayPal, Alipay).

## Groq

`base_url` — `https://api.groq.com/openai/v1` · модель — `openai/gpt-oss-120b` (полегче —
`openai/gpt-oss-20b`). Самая быстрая выдача ответа, настоящий бесплатный тариф: карта не нужна,
есть лимиты запросов в минуту и в сутки. Регистрация на [console.groq.com](https://console.groq.com),
затем [API Keys](https://console.groq.com/keys): создать и скопировать (начинается с `gsk_`).

## Mistral

`base_url` — `https://api.mistral.ai/v1` · модель — `mistral-large-latest` (дешевле —
`mistral-small-latest`). Единственный из четырёх, где бесплатный режим заявлен явно и без карты.
Регистрация на [console.mistral.ai](https://console.mistral.ai), затем
[API keys](https://console.mistral.ai/api-keys): **Create new key**, имя, скопировать; Free-режим
Studio включён сразу, лимиты снимаются в Billing.

Документация: [OpenRouter](https://openrouter.ai/docs/api-reference/authentication),
[DeepSeek](https://api-docs.deepseek.com/), [Groq](https://console.groq.com/docs/openai),
[Mistral](https://docs.mistral.ai/getting-started/quickstarts/developer/first-api-request).

## Куда это вставить

Два текстовых файла в одной папке: на Windows `%APPDATA%\snens\`, на Linux и macOS
`~/.config/snens/`; или то же в окне приложения: «настройки» → «провайдеры». Ключ — в `.env` (одна
строка, без кавычек), адрес и модель — в `config.toml`.

```sh
SNENS_API_KEY=sk-or-v1-твойключ
```

```toml
base_url = "https://openrouter.ai/api/v1"
model = "deepseek/deepseek-v4.1-flash-1017"
```

Два провайдера сразу: `active_provider` — имя того, которым работаешь сейчас.

```toml
active_provider = "deepseek"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
model = "deepseek/deepseek-v4.1-flash-1017"
key_env = "SNENS_OPENROUTER_KEY"

[providers.deepseek]
base_url = "https://api.deepseek.com"
model = "deepseek-flash"
key_env = "SNENS_DEEPSEEK_KEY"
```

```sh
SNENS_OPENROUTER_KEY=sk-or-v1-...
SNENS_DEEPSEEK_KEY=sk-...
```

Спотыкаются чаще всего на трёх: `/chat/completions` на конце адреса (не нужен), имя модели в
написании чужого провайдера, ключ в системных переменных — он перебивает `.env`. Дальше:
[провайдеры и ключи](/docs/providers.html), [с чего начать](/docs/start.html).