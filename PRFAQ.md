# PRFAQ: Broadside

---

## PRESS RELEASE

**FOR IMMEDIATE RELEASE**

**Broadside Gives Organizations a Fediverse Presence in Ten Minutes, No Mastodon Required**

*Seattle, WA* — **Broadside** is a new open-source server that lets any organization publish to the fediverse as a single binary with zero infrastructure dependencies. People on Mastodon, Misskey, GoToSocial, and any other ActivityPub service can follow an organization's broadside account and see its posts in their home timeline — no special client, no account on a corporate platform, no algorithm.

Organizations post content through whichever surface fits their workflow: a CLI command, an RSS feed that broadside polls automatically, a webhook from a CI pipeline, or a markdown file dropped in a directory. Broadside federates the content to followers using standard ActivityPub. From a follower's perspective, the account looks and behaves like any other fediverse account.

"Organizations keep asking how to 'get on Mastodon' and the answer is always 'run a full Mastodon stack or beg for an account on someone else's instance,'" said the project author. "Broadside is the third option: own your identity, publish your content, skip everything else."

A working deployment requires one binary, one config file, one SQLite database, and a reverse proxy for TLS. There is no PostgreSQL, no Redis, no Sidekiq, no Elasticsearch, no Node.js asset pipeline. Memory footprint is under 50 MB. Cold start is under one second.

Broadside supports multiple personas under one domain. A company can run `@announcements@corp.example`, `@engineering@corp.example`, and `@careers@corp.example` as independent ActivityPub actors — each with its own followers, posting cadence, and content source — from a single process on a single machine.

Broadside is released under the AGPL-3.0 license.

---

## FREQUENTLY ASKED QUESTIONS

### Customer / User FAQs

**Q: Who is this for?**

A: Any organization that wants a fediverse presence for broadcasting — press releases, blog posts, product updates, engineering announcements, status page notifications — without operating a full social media server. The organization publishes; followers consume. There is no two-way conversation, no DMs, no moderation queue.

**Q: Can people reply to our posts?**

A: People can reply from their own clients, and their replies are visible to their own followers. Broadside accepts the inbound activity (so remote servers don't retry) but does not store, display, or forward replies. If you need two-way engagement, run smallhold or Mastodon.

**Q: Can we use our existing domain?**

A: Yes. Broadside serves WebFinger and actor documents on your domain. Your accounts are `@name@yourdomain.example`. You own the identity.

**Q: What if we already have a website on that domain?**

A: Your reverse proxy routes `/.well-known/webfinger`, `/.well-known/nodeinfo`, and `/users/*` to broadside. Everything else goes to your existing web server. Broadside does not serve HTML pages or static assets for humans.

**Q: Can people on mastodon.social follow us?**

A: Yes. That is the primary compatibility target. A user on any ActivityPub server searches for `@name@yourdomain.example`, clicks follow, and sees your posts in their home timeline.

**Q: How do we post?**

A: Four options, mix and match:
1. **CLI**: `broadside post --persona=blog "text"` or pipe markdown from stdin
2. **RSS/Atom feed**: point broadside at your blog's feed, it federates new entries automatically
3. **Webhook**: POST JSON to a keyed endpoint from your CI, CMS, or internal tools
4. **Directory watch**: drop a markdown file in a directory, broadside federates and archives it

**Q: Can we schedule posts?**

A: Not directly. Use cron, your CI system, or any scheduler to call the CLI or webhook at the desired time. Broadside publishes immediately on receipt.

**Q: What happens if we stop running broadside?**

A: Followers see no new posts. Their clients do not error. If the server is unreachable for an extended period, some remote servers will eventually unfollow (Mastodon drops followers after repeated delivery failures). Restarting broadside resumes normal operation. Followers do not need to re-follow.

---

### Technical FAQs

**Q: How does this relate to smallhold?**

A: Both projects share the `fieldwork` crate, which provides HTTP signatures, ActivityPub delivery, WebFinger, and actor management. Smallhold is a full Mastodon-compatible server for humans with clients. Broadside is a broadcast appliance for organizations with no client API at all. They are separate binaries with different purposes.

**Q: Why not just run Mastodon and only use it for posting?**

A: You can, but you are deploying and maintaining PostgreSQL, Redis, Sidekiq, and a Rails application to use 5% of its features. You also expose the full Mastodon attack surface — OAuth, client API, moderation endpoints, streaming, admin UI — none of which you need. Broadside's attack surface is: one webhook endpoint (optional, key-authenticated), WebFinger, and an ActivityPub inbox that discards everything.

**Q: Why not use a Mastodon bot on someone else's instance?**

A: Your identity is `@bot@someone-elses-server.example`. You don't control the domain. The instance admin can suspend you. You're consuming resources on infrastructure meant for humans. For a corporate presence, you want `@name@corp.example` on infrastructure you control.

**Q: What's the delivery model?**

A: Same as smallhold. Activities are queued in a SQLite table and processed by an in-process tokio task. Retry schedule: 1m, 5m, 30m, 2h, 8h, 24h. Dead after six failures. Per-domain circuit breaker pauses delivery after ten consecutive failures. `broadside queue inspect` and `broadside queue retry` provide operator visibility.

**Q: How does RSS/Atom ingestion work?**

A: Broadside polls configured feeds on an interval (default 15 minutes). New entries (by `<id>` or `<guid>`) are converted to ActivityPub Notes: title becomes the first line, body is sanitized HTML, linked images become media attachments with blurhash. The feed's last-seen state is persisted in SQLite to survive restarts.

**Q: What about media?**

A: Images only. CLI accepts local file paths. Webhook accepts URLs (broadside fetches and caches). RSS entries use linked images. All images are validated (MIME sniffing, dimension limits, EXIF stripping). No video, no audio. Rejected uploads get a 422 with a clear error.

**Q: How do I back up my data?**

A: Copy the SQLite file and the `media/` directory. That is the entire instance state. `sqlite3 db.sqlite ".backup backup.sqlite"` for a live-consistent copy.

**Q: What about monitoring?**

A: `broadside status` reports: personas, follower counts, queue depth, failed deliveries, circuit breaker state. Expose it to your existing monitoring via cron or a healthcheck endpoint (`GET /health` returns JSON).

---

### Business / Strategic FAQs

**Q: Is there a market for this?**

A: The fediverse is growing and organizations are starting to ask how to participate. The current answers are "run Mastodon" (too heavy) or "get a bot account" (don't own your identity). Broadside is the answer for organizations that want presence without participation. The market is small today but grows with fediverse adoption.

**Q: Could this be a hosted service?**

A: Yes, and more naturally than smallhold. Each customer gets a broadside instance on their domain. The operator provisions a binary, a SQLite file, and a reverse proxy vhost. Per-customer resource consumption is minimal (under 50 MB RAM, near-zero CPU when idle). A managed "fediverse presence as a service" offering is a plausible business model.

**Q: What's the competitive landscape?**

A: Nothing does exactly this. Mastodon, GoToSocial, Pleroma, and Misskey are all interactive social servers. Honk and snac are minimal but still assume a human user. WordPress plugins (ActivityPub for WordPress) federate blog posts but require WordPress. Broadside is the first purpose-built broadcast-only ActivityPub server.

**Q: What's v2?**

A: Candidates: analytics (follower growth, reach estimates per post), post scheduling built in, Webpush notifications to a monitoring channel, and multi-language posts (ActivityPub `contentMap`). All are optional and none change the core architecture.

---

> **What is a PRFAQ?** A PRFAQ (Press Release / FAQ) is an Amazon-originated product planning technique. It starts with a fictional press release written as if the product has already launched successfully, forcing clarity on customer benefit and desired outcome. The FAQ section then anticipates hard internal and external questions. Writing the press release first ensures the team aligns on what success looks like before committing to implementation.
