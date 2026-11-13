# Cloud router — TypeScript

Send Anthropic API traffic through Rayline's **hosted cloud router** using the
official [`@anthropic-ai/sdk`](https://www.npmjs.com/package/@anthropic-ai/sdk)
package. The SDK points at `https://api.rayline.ai` and authenticates with a
Rayline router key.

## Setup

```bash
npm install
```

Requires Node 18+.

## Configure

Generate a router key at <https://platform.rayline.ai/keys>. It looks like
`rlk-...`. Export it as `RAYLINE_API_KEY`; the example exits with an error if it
is not set:

```bash
export RAYLINE_API_KEY=rlk-...
```

> Router keys are **bearer** tokens, so the example passes them via
> `authToken`, not `apiKey`.

## Run

```bash
npx tsx main.ts
```
