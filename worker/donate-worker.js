// Pasture donation Worker — Cloudflare Workers, zero dependencies.
//
// Creates a $1/month Stripe Checkout subscription and verifies webhooks.
// SECRETS ARE NEVER HARDCODED. Provide via `wrangler secret put`:
//   STRIPE_SECRET_KEY      (use sk_test_* until you explicitly go live)
//   STRIPE_PRICE_ID        (a $1/month recurring Price created in Stripe)
//   STRIPE_WEBHOOK_SECRET   (whsec_* from the webhook endpoint)
//   SUCCESS_URL, CANCEL_URL (redirect targets)
//
// Privacy (I5): this Worker stores nothing. It does not persist emails,
// customer IDs, or any PII. The webhook handler only acknowledges events.
//
// LIVE MODE / DEPLOY require explicit human approval (release-approval skill,
// Class C). This file is test-mode-first.

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    try {
      if (request.method === "GET" && url.pathname === "/donate") {
        return await createCheckout(env);
      }
      if (request.method === "POST" && url.pathname === "/webhook") {
        return await handleWebhook(request, env);
      }
      if (url.pathname === "/") {
        return json(200, { service: "pasture-donate", ok: true });
      }
      return json(404, { error: "not found" });
    } catch (err) {
      // Generic message to the client; detail stays in logs (§5.2).
      console.error("pasture-donate error:", err && err.message);
      return json(500, { error: "internal error" });
    }
  },
};

async function createCheckout(env) {
  requireEnv(env, ["STRIPE_SECRET_KEY", "STRIPE_PRICE_ID", "SUCCESS_URL", "CANCEL_URL"]);
  const body = new URLSearchParams();
  body.set("mode", "subscription");
  body.set("line_items[0][price]", env.STRIPE_PRICE_ID);
  body.set("line_items[0][quantity]", "1");
  body.set("success_url", env.SUCCESS_URL);
  body.set("cancel_url", env.CANCEL_URL);

  const res = await fetch("https://api.stripe.com/v1/checkout/sessions", {
    method: "POST",
    headers: {
      Authorization: `Bearer ${env.STRIPE_SECRET_KEY}`,
      "Content-Type": "application/x-www-form-urlencoded",
    },
    body: body.toString(),
  });

  if (!res.ok) {
    console.error("stripe session create failed:", res.status);
    return json(502, { error: "could not create checkout session" });
  }
  const session = await res.json();
  if (!session.url) {
    return json(502, { error: "no checkout url returned" });
  }
  // Redirect the donor straight to Stripe's hosted page.
  return new Response(null, { status: 303, headers: { Location: session.url } });
}

async function handleWebhook(request, env) {
  requireEnv(env, ["STRIPE_WEBHOOK_SECRET"]);
  const sigHeader = request.headers.get("stripe-signature") || "";
  const payload = await request.text();

  const ok = await verifyStripeSignature(payload, sigHeader, env.STRIPE_WEBHOOK_SECRET);
  if (!ok) {
    return json(400, { error: "invalid signature" });
  }
  // Signature valid. We intentionally persist nothing (I5). Acknowledge only.
  return json(200, { received: true });
}

// Verify a Stripe webhook signature: HMAC-SHA256 over `${t}.${payload}`.
async function verifyStripeSignature(payload, header, secret) {
  const parts = Object.fromEntries(
    header.split(",").map((kv) => {
      const i = kv.indexOf("=");
      return [kv.slice(0, i).trim(), kv.slice(i + 1).trim()];
    })
  );
  const t = parts["t"];
  const v1 = parts["v1"];
  if (!t || !v1) return false;

  // Reject timestamps older than 5 minutes (replay protection).
  const now = Math.floor(Date.now() / 1000);
  if (Math.abs(now - Number(t)) > 300) return false;

  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );
  const mac = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(`${t}.${payload}`)
  );
  const expected = toHex(new Uint8Array(mac));
  return timingSafeEqual(expected, v1);
}

function toHex(bytes) {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

function timingSafeEqual(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

function requireEnv(env, keys) {
  for (const k of keys) {
    if (!env[k]) throw new Error(`missing required secret/var: ${k}`);
  }
}

function json(status, obj) {
  return new Response(JSON.stringify(obj), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}
