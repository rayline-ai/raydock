"""Use Rayline's hosted cloud router from Python via the Anthropic SDK.

This points the official `anthropic` client at the hosted Rayline router instead
of api.anthropic.com. The router decides where each request goes and forwards it
on with your provider credentials.

Prerequisites:
  1. pip install -r requirements.txt
  2. Generate a router key at https://platform.rayline.ai/keys. It looks like
     "rlk-...". Export it as RAYLINE_API_KEY.

Run:
  python main.py
"""

import os
import sys

from anthropic import Anthropic

# Hosted Rayline router endpoint.
BASE_URL = "https://api.rayline.ai"

# Your Rayline router key (rlk-...). Router keys are *bearer* tokens, so they are
# passed via `auth_token=` (Authorization: Bearer ...), not `api_key=`.
API_KEY = os.environ.get("RAYLINE_API_KEY")
if not API_KEY:
    sys.exit("RAYLINE_API_KEY is not set. Export your Rayline router key (rlk-...) as RAYLINE_API_KEY.")

client = Anthropic(base_url=BASE_URL, auth_token=API_KEY)

message = client.messages.create(
    # "arc-sage-1.0" lets ARC pick the concrete model. You can also pass
    # a concrete id such as "claude-sonnet-5".
    model="arc-sage-1.0",
    max_tokens=2048,
    messages=[
        {"role": "user", "content": "In one sentence, what is Rayline?"},
    ],
)

for block in message.content:
    if block.type == "text":
        print(block.text)
