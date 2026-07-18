# Security Audit Prompt — Broadside

Run this as a red-team pentest. Check every network input and tainted data source. Trust the database, filesystem, and operator. Everything from the network is hostile.

## Threat model

This is an ActivityPub server facing the open internet. Remote servers, clients, and attackers can send arbitrary HTTP requests to any endpoint. Inbound federation payloads are fully attacker-controlled. Media URLs from remote posts point to attacker-controlled servers.

## What to check

For each finding: file, line number(s), severity (CRITICAL/HIGH/MEDIUM/LOW), one-line description. File each finding as a bead (`bd create --type=bug --priority=N --title="..."`) with severity mapping: CRITICAL=P0, HIGH=P1, MEDIUM=P2, LOW=P3.

### 1. SSRF (Server-Side Request Forgery)
- **Outbound HTTP requests**: Every place the server fetches a URL from untrusted data (actor documents, WebFinger, media URLs, inbox URLs for delivery). Check: is there private IP blocking? Can `http://169.254.169.254/latest/meta-data/`, `http://localhost:8080/`, `http://10.0.0.1/`, `http://[::1]/` be reached?
- **Redirect following**: Is it disabled? A remote server could redirect to an internal URL.
- **DNS rebinding**: Initial DNS resolves to public IP; second resolve goes to private IP during actual connection.

### 2. XSS (Cross-Site Scripting)
- **HTML pages**: Every place user-controlled or remote-controlled data is inserted into HTML. Check: display names, bio HTML, post content, profile field names/values, OpenGraph meta tags, RSS/Atom feed entries.
- **HTML escaping**: Is `ammonia::clean()` or equivalent used? Are HTML attribute values escaped (quotes)?
- **Content-Type**: Are `X-Content-Type-Options: nosniff` headers set?

### 3. HTML sanitization
- **Inbound remote content**: Is `ammonia` used with a restrictive allowlist (not defaults)?
- **Allowed tags**: Should be only: `p, br, a, span, em, strong, del, blockquote, code, pre, ul, ol, li`
- **Allowed attributes**: `href` on `<a>`, `class` on `<a>` and `<span>`, nothing else.
- **Remote display names and bios**: Sanitized before storage?

### 4. HTTP Signature verification
- **`(request-target)` path**: Is the actual request path used, or is it hardcoded to `/inbox`?
- **Date header freshness**: Is the Date header checked to be within ±5 minutes?
- **keyId-to-actor validation**: Does the code verify that the keyId (minus fragment) matches the activity's actor field?
- **Replay attacks**: Can a captured signed request be replayed indefinitely?
- **Key re-fetch SSRF**: When re-fetching an actor key on signature failure, is the URL validated?

### 5. OAuth / Authentication
- **Open redirect**: Is `redirect_uri` validated against the registered app's URI?
- **XSS in authorize form**: Are all query params HTML-escaped before embedding in the HTML form?
- **Auth code reuse**: Is the code deleted atomically (DELETE RETURNING) to prevent race conditions?
- **Password brute force**: Is there rate limiting on the login endpoint?
- **Token timing attack**: Is the token hash comparison done via SQL (acceptable) or via string equality (timing-attackable)?
- **Scope enforcement**: Does the auth middleware check scopes against endpoint requirements?
- **CSRF**: Is the authorize form protected against cross-site form submission?

### 6. Input validation
- **JSON depth limit**: Can deeply nested JSON (10,000 levels) cause stack overflow?
- **String length caps**: Are ALL string fields from remote activities length-capped before DB insertion? (actor URIs, content, display names, bios, tag names, etc.)
- **Integer parsing**: What happens with negative IDs, non-numeric IDs, or MAX_INT?
- **Null bytes**: Are `\x00` bytes in strings rejected?
- **SQL wildcards**: Do LIKE queries escape `%` and `_` from user input?

### 7. Media handling
- **MIME sniffing**: Is the MIME type checked from magic bytes, not just Content-Type header?
- **EXIF stripping**: Are images re-encoded to strip EXIF metadata (GPS, camera info)?
- **Decompression bombs**: Are image decode limits enforced (max pixel count, max memory)?
- **SVG XSS**: Are SVG uploads rejected?
- **Path traversal**: Can crafted filenames write outside the media directory?
- **Polyglot files**: Can a valid JPEG that's also valid HTML bypass checks?

### 8. Delivery worker
- **Inbox URL validation**: Does the delivery worker validate target inbox URLs aren't private IPs?
- **Response body limits**: Are responses from delivery attempts size-limited?
- **Circuit breaker bypass**: Can an attacker reset the circuit breaker by mixing successes and failures?

### 9. Streaming / WebSocket
- **Auth before upgrade**: Is WebSocket authentication checked before the protocol upgrade?
- **Channel isolation**: Can a user subscribe to another user's private channel?
- **Connection limits**: Can an attacker exhaust file descriptors with thousands of idle connections?
- **Broadcast backpressure**: Can event flooding cause OOM via the broadcast channel?

### 10. Rate limiting
- **Inbox deliveries**: Is there per-IP or per-actor rate limiting on inbox POST requests?
- **API endpoints**: Are authenticated endpoints rate-limited?
- **Notification spam**: Can repeated favourite/reblog create unlimited notifications?

### 11. Response security headers
- **Cache-Control**: Are dynamic responses marked `no-store` or `private`?
- **X-Content-Type-Options**: `nosniff` on all responses?
- **X-Frame-Options**: `DENY` to prevent clickjacking?
- **Referrer-Policy**: Set to prevent URL leakage?

### 12. XML injection
- **RSS/Atom feeds**: Are all dynamic values (usernames, display names, post content) XML-escaped?
- **host-meta**: Is the domain value XML-escaped in the XRD template?
- **CDATA breakout**: Can `]]>` in content break out of CDATA sections?

## How to report

For each finding, run:
```bash
bd create --type=bug --priority=N --title="SEVERITY: one-line description" \
  --description="File: path, Lines: N-N. Full explanation of the vulnerability and how to fix it."
```

Then fix each one. Run `cargo test` and `cargo clippy --all-features -- -D warnings` after each batch of fixes.

## Reference

These are the findings from the same audit on smallhold (sister project). Check if broadside has the same issues:

| ID | Severity | Issue |
|----|----------|-------|
| C1 | CRITICAL | SSRF: no private IP blocking on outbound HTTP |
| C2 | CRITICAL | XSS: OAuth authorize form doesn't escape params |
| C3 | CRITICAL | XSS: profile field values not sanitized in HTML |
| C4 | CRITICAL | Open redirect: redirect_uri not validated |
| C5 | CRITICAL | Auth code race: concurrent token exchange |
| C6 | CRITICAL | JSON depth DoS: no recursion limit on inbox parse |
| H1 | HIGH | No Date header freshness check on signatures |
| H2 | HIGH | No response body size limit on outbound fetches |
| H3 | HIGH | Remote actor display_name/bio not sanitized |
| H4 | HIGH | No rate limit on admin password login |
| H5 | HIGH | Token hash comparison not constant-time |
| H7 | HIGH | Unbounded recursive CTE in thread context |
| H8 | HIGH | Notification spam: no dedup or rate limit |
| M2 | MEDIUM | OG meta tags not HTML-entity-escaped |
| M3 | MEDIUM | RSS/Atom usernames not XML-escaped |
| M5 | MEDIUM | Search LIKE wildcards not escaped |
