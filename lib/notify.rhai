// notify.rhai — Generic webhook notification helper (roadmap 2.6).
//
// A thin wrapper over the `http_post` builtin, so a lifecycle hook (see
// lib/deploy.rhai's hook_pre_deploy / hook_post_deploy / hook_post_rollback) has a
// one-line way to post to a webhook (Slack incoming webhook, Discord, a custom
// endpoint, whatever) without hand-writing http_post + JSON escaping in every
// project.
//
// Usage:
//   import "lib/notify" as notify;
//   fn hook_post_deploy(service, image, hosts) {
//       notify::slack(secret("SLACK_WEBHOOK_URL"), service + " " + image + " live on " + hosts.len() + " host(s)");
//   }
//
// `http_post` is already dry-run-safe (never executes in dry-run — recorded as a
// planned "[assumed ok] POST <url>" check, never a real request), so nothing here
// needs its own dry-run handling.
//
// Best-effort by convention, not by force: these functions return the
// `HttpResponse` (so a caller CAN inspect `.ok`/`.status` or `throw` on failure if
// it wants to), but never throw themselves — matching lib/deploy.rhai's own
// established "post-deploy hooks are best-effort" pattern (see
// run_post_deploy_hook's doc comment). A pre_deploy hook that wants to actually
// BLOCK the deploy on a failed notification should check `.ok` itself and throw.

// webhook(url, payload) -> HttpResponse — POST `payload` (an already-serialized
// JSON string) to `url`. `url` may be a Secret (from secret("...")) — a webhook
// URL usually functions as a bearer credential (anyone who has it can post to
// it), so treating it like one is deliberate. `http_post` itself only accepts a
// plain string, so this reveals a Secret internally, right before the call — a
// caller does NOT need to (and normally should not) call reveal() itself. The
// revealed URL still stays out of the dry-run plan: `ctx.record` (the layer
// http_post's dry-run recording goes through) redacts every registered
// secret's plaintext from whatever it's given, so the "[assumed ok] POST
// <url>" line shows the redacted form, not the real URL.
fn webhook(url, payload) {
    let real_url = if type_of(url) == "Secret" { reveal(url) } else { url };
    http_post(real_url, payload)
}

// slack(url, text) -> HttpResponse — POST a Slack "incoming webhook"-shaped
// payload (`{"text": "..."}`) to `url`. `text` is JSON-string-escaped via
// json_string (quotes/newlines/control characters) — the same helper
// lib/caddy.rhai already uses for JSON body construction — so a message
// containing arbitrary characters (a service name, an error message) can never
// produce malformed JSON.
fn slack(url, text) {
    webhook(url, "{\"text\":" + json_string(text) + "}")
}
