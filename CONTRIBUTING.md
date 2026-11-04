# Contributing

## Public content policy

This project is public. Everything committed here is readable by anyone, so the
public surface is limited to objective, current facts.

**README**

- English first, followed by a Simplified Chinese section that describes the
  same features. No other language pairs.
- Describes **what the software does today**: features, install, usage,
  architecture, build steps. No roadmap, plans, dates, priorities, status
  reports, or announcements.

**Documentation (`docs/`)**

- Describes the current architecture, operation and behaviour of the software.
- No meeting notes, design discussions, decision logs with rationale from
  private conversations, personal context, or links to internal material.
- Architecture decision records state the decision and its technical
  consequences only.

**Release notes**

- English only, objective list of changes. No discussion, no commentary about
  the team, and no forward-looking claims.

**Never commit**

- Personal paths, usernames, machine names, media library contents, credentials
  or tokens — including in comments inside `.gitignore` and example files.
- References to private chats, internal documents or unpublished plans.
- Generated artifacts, local databases, logs or scratch directories.

**Where discussions live**

Design discussions happen in private channels. Their outcome reaches this
repository only as a refined decision or a code change; the discussion itself
is not committed.

## Code contributions

- Keep the component boundaries: `web -> api -> media -> kernel`.
- Run the component's checks before committing (`cargo fmt/clippy/test` for Rust,
  `npm test` and `npm run build` for the UI).
- Commit messages describe the change, not the conversation around it.
