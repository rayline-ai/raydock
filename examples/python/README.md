# Cloud router — Python

Send Anthropic API traffic through Rayline's **hosted cloud router** using the
official [`anthropic`](https://pypi.org/project/anthropic/) Python SDK. The SDK
points at `https://api.rayline.ai` and authenticates with a Rayline router key.

## Setup

```bash
pip install -r requirements.txt
```

## Configure

Generate a router key at <https://platform.rayline.ai/keys>. It looks like
`rlk-...`. Export it as `RAYLINE_API_KEY`; the example exits with an error if it
is not set:

```bash
export RAYLINE_API_KEY=rlk-...
```

> Router keys are **bearer** tokens, so the example passes them via
> `auth_token=`, not `api_key=`.

## Run

```bash
python main.py
```
