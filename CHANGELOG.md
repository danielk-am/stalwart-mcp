# Changelog

## Unreleased (tracking fork)

- Add account discovery as a tool.
- Select defaults in configuration order; reject duplicate names and unavailable explicit defaults.
- Disable permanent deletion unless explicitly enabled.
- Validate each JMAP mutation result before reporting success; warn on partial bulk failure.
- Document stdio-only installation and unresolved upstream HTTP authorization limitations.

## [0.2.0] - 2026-04-15

### Added
- `mail_bulk_read` tool for bulk marking emails as read in a single JMAP request
- `mail_bulk_delete` tool for permanently deleting multiple emails in one JMAP request
- Rate limiter entries for all bulk operations and spam training

### Fixed
- Missing rate limiters for `mail_bulk_junk`, `mail_bulk_delete`, `mail_bulk_read`, and `spam_train`

## [0.1.0] - 2026-04-13

### Added
- JMAP mail client with multi-account support
- Read tools: `mail_list_folders`, `mail_list_recent`, `mail_search`, `mail_get`
- Write tools: `mail_flag`, `mail_move`, `mail_draft`, `mail_send`
- `mail_bulk_junk` tool for bulk marking emails as junk/notjunk
- `spam_train` tool for training Stalwart's Bayes classifier via admin API
- MCP Prompts: `triage_inbox`, `draft_reply`, `search_and_summarize`, `train_spam_filter`
- MCP Resources: accounts list, folder list, message content
- Real-time notifications via JMAP EventSource (SSE)
- Per-tool rate limiting with token bucket algorithm
- JWT authentication for remote HTTP connections
- Dual transport: stdio (local) and Streamable HTTP (remote)
- Structured audit logging with tracing
- Pagination support with offset parameter
