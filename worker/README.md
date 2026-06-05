# Pasture donation Worker

Serverless $1/month Stripe donation endpoint for Pasture. Zero dependencies,
stores no PII.

## Endpoints
- `GET /donate`  — creates a Stripe Checkout subscription, 303-redirects to it.
- `POST /webhook` — verifies the Stripe signature (HMAC-SHA256, 5-min replay
  window) and acknowledges. Persists nothing.
- `GET /`        — health/info.

## Setup (test mode first)
1. In Stripe (test mode), create a recurring Price of $1/month; note its
   `price_...` id.
2. `cp wrangler.toml.example wrangler.toml` and set SUCCESS_URL / CANCEL_URL.
3. Set secrets (test keys):
   ```
   wrangler secret put STRIPE_SECRET_KEY     # sk_test_...
   wrangler secret put STRIPE_PRICE_ID       # price_...
   wrangler secret put STRIPE_WEBHOOK_SECRET # whsec_...
   ```
4. `wrangler deploy` (test). Point Pasture at it:
   `export PASTURE_DONATE_URL=https://<your-worker>/donate`

## Going live  ⚠ requires explicit approval
Switching to `sk_live_*`, creating the production webhook, and the public
deploy are irreversible/billing-sensitive steps (release-approval skill,
Class C). Do these only after a deliberate go decision.
