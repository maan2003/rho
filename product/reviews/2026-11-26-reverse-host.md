# Review: agent host and client/host protocols (reverse)

- **Date:** 2026-11-26
- **Commit:** `561e6d43b`
- **Device:** static (`product/` + `agent-host/*` + every `protocol` module named by `rho_rpc::protocol::Protocol`)
- **Harness / captures:** none (static). Test runs on this commit, from `/src/rho`:
  - `cargo build --tests --keep-going -p rho-agent -p rho-agent-host -p rho-cli -p rho-chat -p rho-agents-client -p rho-agent-types -p rho-ledger -p rho-ledger-server -p rho-rpc -p rho-agent-hosts -p rho-agents-view -p rho-files -p rho-git -p rho-claude -p rho-devshell -p rho-ci -p rho-shell-view -p rho-terminal -p rho-desktop-client -p rho-rtc`
  - `cargo test` on the same packages (`--no-fail-fast`), `cargo test -p rho-agent-host --test '*'` after `cargo build -p rho-agent-host`
  - Not run: the Python tests in `agent-host/rho-notebook/src/rho/tests` (they need the `_rho` harness).

## Summary

The host's protocol split is clean: each protocol lives in its client crate, the host dispatches on one `Protocol` enum, and the chat log, projects, flashes and thread work cross the wire in the product's terms. The worst problem is that **the retired mail model is still the agents' working API**: every agent gets `mail` beside `chat`, `mail.send` pings by default, can send a kept message that does not ping, and addresses a writer. The Advisor and child prompts teach it, and the fake model speaks it. The wire types under it (`Mention::User { ping }`, `reply_to`, `Post.mentions`) still represent the kinds `chat.md` rejected on 2026-11-21. Two lib test targets on `main` do not compile and three tests fail for stale reasons. All five trace to the mail→chat and catalogue changes, not to new bugs.

## Findings (ranked by `runs × cost`)

### 1. Agents still get the retired `mail` API, beside `chat`
- **Kind:** code (zombie)
- **Level:** high-level
- **Status:** open
- **Where:** `chat.Message` (`ping`), `chat.Flash`, `chat.the last message stays live`; agent-facing Python and prompts
- **Evidence:**
  - `agent-host/rho-notebook/src/rho/tools.py:526` gives every agent `{"mail": Mail(...), "chat": Chat(...)}`. `Chat` subclasses `Mail` (`tools.py:267`).
  - `Mail.send` (`tools.py:96`, `mail.py:47`): `ping=None` pings by default, `ping=False` posts a kept message that does not ping, `reply_to=`/`to=None` addresses the writer, `replace_last=True` edits in place. The product rejected each: `chat.md` Message `rejected:` "a message that is kept but does not ping", "a reply with an addressee", and Flash `rejected:` "an `ephemeral=True` flag with `False` as the default · a default lets the model skip the choice".
  - `spawn_user_owned_engineer` writes the brief with `ping=False` (`tools.py:471`).
  - Prompts still teach it: `prompt/claude/advisor.md:5,42,236,405`, `prompt/claude/child.md:159-161`, `prompt/gpt/child.md:132`, `prompt.py:60,80,101` (`mail.send(None, text, reply_to=mail_id)`).
  - The inbound header the agent reads, and the transcript the user reads, says `Mail from the user:` (`rho/default.py:97`). `rho-agents-view/src/render.rs:460,688,732` parses `Mail from`, `mail.send(` and the gone `mail.status(`.
  - The host keeps a separate refusal text for `api == "mail"` (`rho-agent/src/host/pool/mail.rs:253`) and two legacy IPC requests, `SendMail`/`SendMail2` with `PyReply::SentMail` (`rho-agent/src/ipc/workset.rs:69-88,169-171,194-198`), marked "remove both at the next necessary epoch bump".
  - The fake model writes `mail.send("user", …)` (`rho-fake-model/src/lib.rs:822,830,890`). See finding 3d for the test this breaks.
- **Cost:** about 100 a day (`chat.Answer a message`). Each message an agent sends through `mail` skips the message-or-flash choice. It deals a card by default, or keeps a message that never deals. This breaks `chat.A message is the record, a flash is the now`.
- **Direction:** Give children and Advisors a chat-shaped way to reach their parent (the `agents.message` path already posts direct mail through `Messages`). Rewrite the prompts and the fake model to use `chat`/`agents`. Then remove `Mail`, `SendMail`/`SendMail2`/`SentMail`, the `api` parameter, and the `mail.*` render parsers, or keep only the parsers for old recordings. Rename the inbound header to `Message from`.

### 2. The message model still represents the kinds the product rejected
- **Kind:** code (structural)
- **Level:** high-level
- **Status:** open
- **Where:** `chat.Message`, `chat.Flash`; `rho_agent_types::chat`, `rho_chat::protocol::Post`
- **Evidence:**
  - Whether a message deals is two fields that must agree: `MessageKind::{Kept, Flash}` and `Mention::User { ping: bool }` (`crates/rho-agent-types/src/chat.rs:78-88`, `pings_user` at `:373`). `Revised { ping: Option<bool> }` (`:168`) and `PyRequest::Edit { ping }` can switch a kept message's ping on or off. `chat.md` says "every message from the project pings the user".
  - Three relation fields on `Posted`: `parent`, `reply_to` ("the original failed start on retry, or a historical addressee", `:118`), and `thread_reply.settles`. The doc on `Revised` still says "Replies to it go to the writer of these words" (`:162`).
  - `Mention::Agent` and `Mention::Direct` route delivery to an addressee. `Post.mentions: Vec<AgentId>` (`crates/rho-chat/src/protocol.rs:67`) is always empty from the GUI (`rho-chat/src/ui/mod.rs:511`, `outbox.rs:245,336,364,404`). Only a test fills it.
- **Cost:** the same 100 a day. Every reader of the log (`cards.rs`, the dealer, the host's live-message rule) must handle the rejected "kept, silent" state. Finding 1 exists because the wire allows it.
- **Direction:** Make the kind one enum: `Message` (always pings) or `Flash`. Drop `Mention::User`, `Revised.ping`, `Edit.ping` and `Post.mentions`. Rename `reply_to` to `retry_of` for its one live use. **Cost of the change:** wire break (bump the epoch in `rho_rpc::protocol`) and a one-hop migration of the host's chat log. Senax skips removed fields, but rows that contain a removed enum variant fail to decode (`crates/senax-encoder/specification.md` §6.2). The migration must also decide what existing `Kept` + `ping: false` rows become, for example messages that are already settled.

### 3. Five tests fail on `main` for stale reasons
- **Kind:** code
- **Level:** high-level
- **Status:** open
- **Where:** test code left behind by the mail→chat and runtime-catalogue changes
- **Evidence and root causes:**
  - **a. `rho-agent` lib tests do not compile** (`E0559`, `E0599`). Commit `003ecf05c` (2026-11-21, "Send user replies to the whole chat thread") added `thread_reply: None` to every `ChatEvent::Posted` literal. One of them, `store/mail.rs:226`, is in `mod prior_ping`, which declares the old `Posted` shape without that field, on purpose. The same commit removed `MailWrite::mail_writer`, but `store/tests.rs:1040` still asserts on it ("a reply can still identify the writer"). The writer-addressed reply is gone, so the assertion has no meaning now. `prior_ping` is a one-hop migration from `f34338421` (2026-11-20). The `temp-migration` skill says to remove such code after one or two commits.
  - **b. `rho-agent-host` lib tests do not compile** (`E0063`): `src/realtime/thread.rs:229` builds a `Message` without `thread_reply`, from the same commit. This module serves the voice protocol, which no client uses (finding 8).
  - **c. `rho-agents-view::system::surface::tests::a_stopped_agent_without_history_can_choose_another_kind`**: `left: 39, right: 35` at `surface.rs:663`. Commit `4a42786ad` (2026-11-23) added `neon` to `NATIVE_MODELS` (`crates/rho-agent-types/src/project.rs:92`). That adds 4 native choices. `neon-high` also moved into `RUNTIMES`, so the usual list grows by one and the remainder shrinks by one. The math: 7 usual + (20 Claude + 20 native − 8 usual) = 39. The test hard-codes the catalogue size. Its sibling hard-codes `19` and breaks the same way when a Claude model is added.
  - **d. `rho-cli --test agent_e2e`** (added today in `d364ac504`): `agent_e2e.rs:277` expects one flash, `working...`, and gets 0. The test's `init.py` installs a `_MailCompat` that posts a flash, but only `if "mail" not in ns` (`agent_e2e.rs:78-84`). `tools.exports` always provides `mail` (finding 1), so the shim never runs. The fake model's `mail.send("user", …)` posts a message and no flash.
  - **e. Environment, not code:** `rho-cli --test wayland_record` fails with `vpx_codec_enc_init_ver() failed: code=3, reason=ABI version mismatch`. `rho-agent-host --test iroh_pairing` passed once and failed once with `No addressing information available` (iroh address lookup). I did not find the root cause of either.
  - Every other test target in the packages above passed, including the `rho-agent-host` integration tests `mail`, `reexec`, `terminal_stream` and `wayland_stream`.
- **Cost:** `just test` and `cargo check --workspace --all-targets` fail on `main`. Since 2026-11-21, nothing has run the `rho-agent` store tests, which cover the live-message rule and flash expiry. This repo has no CI workflow file.
- **Direction:** (a) delete `prior_ping` and `migrate_pings`, or at least the stale field, and delete the `mail_writer` assertion. (b) goes away with finding 8. (c) derive the expectation independently, for example as the set of `Runtime::choices` of both kinds minus `current`, with no duplicates. Do not use a count. (d) fix it with finding 1, or make the shim replace `mail` unconditionally.

### 4. A project is a root message of the chat forest, and its settings live elsewhere
- **Kind:** code (structural)
- **Level:** high-level
- **Status:** open
- **Where:** `chat.Project`, `chat.Start a project`
- **Evidence:** `ProjectId(pub MessageId)` (`project.rs:12`). `store/projects.rs:1-2` says "Names and identity still belong to the roots of the mail forest". The name is a `Named` event on the root, and place, mode and runtime are in a separate `mail_projects` table. `rho_chat::protocol::Project.root` is still `#[senax(rename = "channel")]`. Agents create projects as bare roots with no settings (`Chat.start` → `mail.make_project`, `tools.py:293`). Peer mail with no project falls back to an `Agent mail` root (`store/mail.rs:46,943`). `ImportProject` (`protocol.rs:43`) is a temp migration of device-only places from `173807e3b` (2026-11-21).
- **Cost:** `Start a project` (1 a week) and every `Write an idea` (12 a day). Projects that an agent creates fall into the "older project … says `no place set`" case. This contradicts `chat.Project` purpose: "one answer, asked once".
- **Direction:** Make a project its own record (name, place, mode, runtime), keyed by its own id, with the root message as its anchor. Refuse agent-created projects without settings, or give them the creating agent's place. Remove `ImportProject` and `legacy_places`.

### 5. A host-held GitHub token pushes without the user's approval
- **Kind:** product (undocumented)
- **Level:** high-level
- **Status:** open
- **Where:** `agents.Approve a push or a fetch`, `agents.a push waits for the user`, `hosts.The window holds the secrets`
- **Evidence:** `git-remote-octo` asks `GitTransportPolicy` (`octo-cli/src/bin/git-remote-octo.rs:24-30`). The host answers true when `platform_secrets` has `GITHUB_TOKEN` (`rho-agent-host/src/host.rs:164-168`). The push then goes through `octo-server` with the token (`octo-server/src/api/git.rs:84-130`), and the window never asks. `PlatformSecretsSet` installs Slack, Notion, Google and GitHub secrets on the host from `rho-cli`.
- **Cost:** about 3 a day (`Approve a push or a fetch`). The rule covers only "the user's ssh login", so a token push is outside the product, and the user cannot see it.
- **Direction:** Decide: (1) document host-held tokens as a second, unprompted path, and name what they may push; or (2) route token pushes through the same approval; or (3) remove the token path.

### 6. The `start agent` menu is a second, undocumented way to start work
- **Kind:** product (undocumented)
- **Level:** high-level
- **Status:** open
- **Where:** `chat.Project.receive an idea`, `agents.Workset`
- **Evidence:** `crates/rho-agents-view/src/system/start.rs:39-160` adds `start agent` and `send and start agent with options` to the palette on a thread. Its rows are `new workset on a repository`, `here, in a directory on the host`, and `join <subject>`, each followed by a runtime prompt and a policy `Function` name. The menu calls `Worksets::StartFromMail` (`worksets.rs:63`). `chat.md` rejected "the model chosen only through `start agent` · that path asks the place again and never makes a project thread". `agents.md` says of `start agent here`: "no command has that name".
- **Cost:** rare, but it asks the place again and bypasses the project's mode, which `chat.Project` says is asked once.
- **Direction:** Cut it, because `on:` and `project › works on…` cover it. Or document `join` as the one rare way to add an agent to an existing workset, which `Workset has: many Agent` implies but no flow provides.

### 7. The runtime catalogue is compiled into both sides, not asked of the host
- **Kind:** code (structural)
- **Level:** high-level
- **Status:** open
- **Where:** `agents.Agent.set runtime`, `chat.Project.project › runtime…`, `chat.Write an idea` (`on:`)
- **Evidence:** `NATIVE_MODELS`, `CLAUDE_MODELS` and `RUNTIMES` are constants in `crates/rho-agent-types/src/project.rs:92-114`, "as `_rho.pyi` lists them". No client protocol call returns the host's runtimes. `PyRequest::Models` serves only agents. The product says "with the host's runtimes as answers". `snippets::cycle_runtime` (`snippets.rs:83`) has no caller.
- **Cost:** each new model needs a GUI release on every device. A GUI built before `neon` refuses `neon-high` on a host that has it, and a newer GUI offers it to an older host. Finding 3c is the symptom in tests.
- **Direction:** Add the catalogue to the `Projects` frame or `Hello`, and validate labels against it. Delete `cycle_runtime`.

### 8. Retired protocols, crates and endpoints still on the wire
- **Kind:** code (zombie)
- **Level:** high-level
- **Status:** open
- **Where:** `rho_rpc::protocol::Protocol` and the protocols it names
- **Evidence (each item serves nothing in `product/`):**
  - `Protocol::Agents`, `Protocol::Ledger`, `Protocol::Workspace` (`rho-rpc/src/protocol/mod.rs:169,178,186`): the host only refuses them (`rho-agent-host/src/lib.rs:1324-1325,1343`). Two tests use `Protocol::Agents` as "some other protocol" (`rho-agent-hosts/src/protocol.rs:318`, `rho-rpc/src/protocol/mod.rs:665`).
  - `crates/rho-files` (1,018 lines, 791 of them `zed_remote.rs`): the protocol is no longer served. Its only user is the `rho-cli` protocol-log printer (`rho-cli/src/lib.rs:100`).
  - **Voice:** `Protocol::Voice` is served by `rho-agent-host/src/realtime.rs` and `realtime/thread.rs` (843 lines), backed by `rho-openai-realtime` (826 lines) and `rho-rtc` (609 lines). No GUI crate depends on `rho-rtc`, since `c71816dce` (2026-11-15) moved the GUI voice into `rho-gui-old`. `rho-cli` uses it only to print protocol logs (`lib.rs:99`). `product/` has no voice.
  - Calls with no caller outside tests: `Worksets::RestartWorkset` (only `tests/mail.rs:1260`, `tests/reexec.rs:373`), `Worksets::SetAuthAccountEnabled`, `Host::ShutdownIfIdle`, `Chat::Reparent`, `Chat::Leave`, `Chat::ResolveProject`. The field `Chat::Post.mentions` is also unused (finding 2).
  - `RequestNotice::ThreadCold` (`rho-agent/src/entry.rs:195-228`) is never built. Its text tells agents to write a `## recap`, which `chat.md` rejected.
  - `snippets.rs:152-153` has a doc comment for a removed rewind snippet, left on `new_cache_key`.
  - `agent-host/specs/ARCH-agent-host.md:180-192` still describes mention kinds `ask`/`result`/`fyi`, `rho.mail.send` with a recipient, and "The GUI starts an agent from an existing message".
- **Cost:** none to the user directly. A served voice endpoint with no client is attack surface for an authenticated client. Each zombie variant must be read past whenever the protocol changes.
- **Direction:** Remove all of them in one change, with one epoch bump, as `agent-host/AGENTS.md` requires for any protocol change. Before you remove `RequestNotice::ThreadCold`, check whether agent journals from 2026-11-20 and 2026-11-21 contain it, because a removed variant fails to decode. Update `ARCH-agent-host.md` in the same change.

### 9. The transcript protocol is per agent, not per workset
- **Kind:** code (structural, already noted)
- **Level:** high-level
- **Status:** open
- **Where:** `agents.Workset`, `agents.Transcript`, `agents.Look into the work`
- **Evidence:** `Worksets::AgentLog` reads "one agent's rows"; `AgentLogPage` says "its workset's other rows are not here" (`crates/rho-agents-client/src/protocol/worksets.rs:47,178`). `agents.md` Workset `status:` already says that transcripts open per agent.
- **Cost:** about 15 a day (`Look into the work`). When several agents work in one workset, the user opens each transcript in turn.
- **Direction:** Add a workset-wide log read with an optional agent filter, so the screen follows `Workset.filter by agent`.

### 10. Host features that only the CLI reaches, and one undocumented palette command
- **Kind:** product (undocumented)
- **Level:** high-level
- **Status:** fixed (2026-11-26, Loop 2 Wave C5 branch `loop2/wc5-gui-accounts-gccp`: global `switch claude account…` and `hosts › gccp › list`, `approve…`, `remove…` in the palette; rename and the performance report were documented in `0ebdae409`)
- **Where:** `usage.Chart`, `hosts.Host`, `chat.Thread`
- **Evidence:**
  - Account switching: `Worksets::ClaudeAccounts`/`SetClaudeAccount` are called only by `rho-cli`. Yet `usage.md` Chart `principle:` says the user can "start the next agent on an account with room", and no product action does that.
  - Agents on other harnesses (GCCP): `Host::GccpList/GccpApprove/GccpRemove` and `rho iroh approve --role gccp`. `product/` has no word for them.
  - `name this thread` in the palette (`rho-chat/src/ui/mod.rs:897`, `Chat::Name`). `chat.Thread` has no rename.
  - `Host::GuiTelemetryUpload` (`rho-agent-hosts/src/connection.rs`): the window uploads performance snapshots to the host.
- **Cost:** `Check what is left` (2 a week) cannot end in its stated action from the window.
- **Direction:** For each one, document it (accounts as a `usage` action, rename as a `Thread` command) or leave it as CLI-only operator tooling that `product/` names as out of scope.
