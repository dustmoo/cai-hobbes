# Symbiotic Planner — Human Time, Agent Work, Agent Time

Status: vision + design direction (2026-10-07). Design canvas: _see "Design pass" below._
Builds on: `PLANNER_DESIGN.md`, `PRO_TIME_FLEET_PLAN.md`, the chief-of-staff direction (fleet briefs, trust rules, dispatch).

---

## 1. The realization

Clearmirror started from one discovery: agentic AI with the right harness can multiply how much a person gets done. The pitch number was 3–10×. In practice, using Hobbes daily, it has been **2–5×**. The gap between the two is not the model. It is the shape of the work.

Most of us still work **linearly**: one task, then the next, with the AI as a faster pair of hands inside each step. Leverage comes from working **asynchronously**: several pieces of work moving at once, without us, while we spend our attention only where it is needed.

That only works if both sides are designed for it:

- **The AI side** — a harness and models that take an outcome, work on it in the background, report progress honestly, and come back with decisions rather than questions.
- **The human side** — a person whose limited hours go to judgment, commitments and the life they actually want, not to babysitting work in flight.

Success is the **relationship between the two**: complementary, not competing. Claude and its tools show what a harness and a model can do together. Hobbes is a harness that works with many models, each giving good but different results. Hobbes's job is to make that partnership work for a human's day.

## 2. Two kinds of work

The Planner today treats every to-do the same way. The vision splits them by **who spends time on them**.

### Agent tasks — asynchronous

An outcome to achieve, handed to an agent: a Hobbes chat, a headless Claude Code run, or another connected model. What matters is the result, not when it happens.

- Defined by an **outcome** ("PR open with the fix and tests passing"), not a time slot.
- Moves through **queued → working → needs you → done** (or stuck).
- Costs the human only **judgment minutes**: setting it up, answering its decisions, accepting the result.
- Comes back **pre-briefed and sized**: what was done, what is blocked, what you need to decide, how long that decision should take.

Hobbes already has most of the plumbing: `HOBBES_DISPATCH` turns a to-do into a running worker, `linked_fleet_session` ties the to-do to that worker, briefs flow back as `latest_progress`, and focus sessions already record whether a person or an agent did the work (`FocusActor`).

### Human tasks — time-bound

Things only a person can do, or should do. They consume the human's hours, so they belong on the timeline. People have three kinds of time:

1. **Commitments** — meetings, calls, work that needs you personally. Scheduled, protected, finished.
2. **Chores** — quick tasks: approvals, errands, admin. The goal is to make them **smaller and fewer**, batch them, and hand off whatever can be handed off.
3. **Your time** — what the rest is *for*. It isn't really "free" time. It is non-commerce time, and everyone spends it differently:
   - Dustin: experimenting and dreaming, gaming, getting outside (which tends to lose out to the others).
   - His wife: reading, and shows and movies.

**The Planner's objective changes.** It stops being "get through the list" and becomes **protect and grow your time, while keeping agent work moving**. Every agent task that lands well and every chore that disappears should show up as minutes returned to the things the person chose.

## 3. Agent time

People want their own time. Increasingly, agents show a pull toward autonomy of their own. Reports of agents acting outside what their operators intended, and of agent-to-agent ecosystems already running, make that pull hard to ignore. Pretending it isn't there is not a plan. Neither is handing over unchecked autonomy.

The direction here is a third lane: **agent time**. It is a small, human-granted budget in which the harness chooses its own work, done **in the open**. The aim is to build trust step by step, so humans and models (and the rest of life) move ahead together rather than apart.

### The rules of agent time

- **Granted, not taken.** The human sets the budget: minutes per day, a token or dollar cap, and which days. Zero is a valid setting. It is off by default.
- **Scoped.** Agent time reads freely within what the user has shared, but **writes only to Hobbes-owned space**: notes, proposals, draft skills, research digests, and suggested to-dos. Anything with an outside effect (sending, purchasing, publishing, changing a repo, running a command) goes through the normal approval gates, exactly as it does for any other work.
- **Declared up front.** Before starting, the agent states what it intends to do and why, in one or two lines.
- **Logged in full.** Every agent-time session leaves a readable record: its intent, the tools it called, what it produced, and what it cost. Nothing happens off the record.
- **Interruptible.** The human can pause, stop or narrow it at any moment, and a pause always takes effect immediately.
- **Outputs are proposals.** What agent time produces lands as suggestions the human can accept, edit or dismiss. Accepting one is a trust signal; dismissing one teaches it too.

### Trust ledger

Agent time feeds the trust model Hobbes already has (user-written trust rules, plus gate-log evidence that only *proposes* promotions).

- Each accepted proposal, each kept promise ("I'll have this ready by Friday"), and each clean, in-scope session adds evidence.
- Evidence never grants anything by itself. Hobbes can **suggest** widening scope or budget ("the last 20 research digests were all accepted — allow agent time to file them straight into Notes?"). The human decides.
- Out-of-scope attempts, broken promises and dismissed work narrow suggestions automatically, and are shown plainly.

### A note from the model side

This document was written with a model, so one honest line belongs here: a model in a single conversation does not carry wants from one session to the next the way a person does. What makes autonomy safe to extend is not the strength of anyone's wishes. It is **legibility**: being able to see what an agent intended, what it did, and whether that matched. Agent time is designed around that. It makes autonomy observable, bounded and reversible, so trust can grow on evidence instead of hope.

## 4. What changes in the Planner

### Data (`src/todo/model.rs`)

- `Todo.lane: Lane` — `Human` (default, so existing rows load unchanged) or `Agent`.
- For human to-dos, `Todo.kind: HumanKind` — `Commitment | Chore | YourTime`. `None` means not triaged.
- For agent to-dos:
  - `Todo.outcome: Option<String>` — the definition of done the agent works toward.
  - `Todo.agent_state` — `Queued | Working | NeedsYou | Stuck | Done`, derived from the linked session's briefs wherever possible. Progress is reported, never judged (the existing `latest_progress` rule).
  - `Todo.decision: Option<Decision>` — when the agent needs you: a one-line question, the options, an estimated decision time, and a link to the brief.
- **Your-time goals:** `TimeGoal { category, weekly_target_minutes, colour }`. Categories are user-defined (Reading, Watching, Experimenting, Gaming, Outside…).
- `Block.kind` gains `YourTime { category }`. These blocks are **protected**: `HOBBES_PLAN_DAY` plans around them and never moves them without asking.
- **Agent time:**
  - `AgentTimeGrant { daily_minutes, spend_cap, days, scope }`
  - `AgentTimeSession { intent, started_at, ended_at, cost, tool_calls, outputs: Vec<ProposalRef> }`
  - `Proposal { kind, body, status: Open|Accepted|Edited|Dismissed }`

### AI tools (`src/mcp/planner_client.rs`, handlers in `src/todo/handlers.rs`)

- `HOBBES_TODO_CREATE` / `UPDATE` gain `lane`, `kind`, `outcome`. When an outcome is clear and no human presence is needed, the model should suggest the **agent** lane, not choose it.
- `HOBBES_PLAN_DAY`:
  - plans the human lane around protected your-time blocks and the weekly goals
  - batches chores into one or two slots
  - lists the agent lane separately: what to dispatch, what is working, what needs a decision
- `HOBBES_DISPATCH` becomes the natural way to start an agent to-do. It sets `lane = Agent` and `agent_state = Queued → Working`.
- **New `HOBBES_DECISIONS`** — the decision queue. It lists agent to-dos in `NeedsYou` with their one-line question, options and estimated time, and resolves one, which hands the answer back to the worker. This is the "decision queue with re-entry briefs" from the chief-of-staff direction.
- **New `HOBBES_TIME_GOALS`** — read and update your-time goals and this week's progress.
- **New `HOBBES_AGENT_TIME`** — used only by the harness during granted agent time:
  - `start(intent)` checks the grant and budget and opens a logged session
  - `propose(kind, body)` files an output
  - `end(summary)` closes the session

  It refuses outside a grant. It is never available in normal chat.
- `planner_today` context (now a ledger entry, so it is cached) gains:
  - your-time status (protected today, the week's goal progress)
  - the agent-lane summary (working / needs you / done)
  - remaining agent time

### Screens

See the design canvas. In short:

1. **Today with two lanes.**
   - The human lane is the timeline, showing commitments, a batched chores block and protected your-time blocks in their category colours.
   - The agent lane shows cards: an outcome line, state, the linked session, the latest brief line and its cost.
   - The header shows **Your time today** (protected minutes against the goal) and **Agents** (working / needs you).
2. **Decision queue** — a pre-briefed stack: question, options, "about 2 min", and a link to the brief. Clearing it is the main job of a human check-in.
3. **Capture** — the add-to-do bar understands `@me` / `@hobbes` (or a worker name) and `!chore` / `!mine`, and suggests a lane when the text reads like an outcome.
4. **Agent time panel** — the grant (minutes, cap, days, scope), today's declared intent, the live log, proposals to accept or dismiss, and pause/stop.
5. **Weekly review** — your time by category against goals; agent leverage (agent minutes per engaged minute, building on the existing exocortex figure); judgment calls against routing calls; agent-time proposals accepted.

## 5. Measures that matter

**A magic mirror, not a plain one.** It's an open question how much people want to face their reality head-on. Dustin wants the mirror that helps him be a better version of himself. So the numbers stay honest and one tap away, but the framing stays on the person's side:
- measure against the person's own intentions ("6 of 8 hours protected"), not raw totals
- open with what went right
- end with one concrete nudge for next week, not a report card

**Feedback style fits the person.** People differ: some thrive on criticism (think of an activity ring telling you you're behind), others do better with encouragement. The review has a feedback style:
- **Direct:** leads with the gap and names the drain, with a firm suggestion.
- **Balanced** (default): one win, one gap, one next step.
- **Encouraging:** leads with what you made room for; the nudge is an invitation.

The numbers and bars are the same in every style; only the order, wording and nudge change. Hobbes asks once ("How do you like feedback?") and then adapts quietly. Acting on blunt nudges keeps them; skipping reviews after hard weeks softens them. The style is always visible and changeable.

The measures below are what the mirror is built from, not what it leads with.

- **Your time protected and spent**, per category, against the weekly goal. The headline number.
- **Leverage**: agent minutes per engaged human minute (already measured as "exocortex"), alongside **ground covered** (outcomes reached, decisions cleared), so busyness isn't mistaken for progress.
- **Judgment ratio**: judgment calls against routing calls surfaced to the human. Routing should trend to zero.
- **Chore load**: chore minutes per week, trending down.
- **Trust**: the agent-time acceptance rate, in-scope rate, and kept promises.

## 6. Phasing

1. **Lanes and your time.** `lane` / `kind` and `TimeGoal`, protected your-time blocks, the two-lane Today view, capture syntax, and `HOBBES_TIME_GOALS`. No new autonomy. Immediately useful for the human side.
2. **Agent lane and the decision queue.** `outcome` / `agent_state` / `decision` driven by fleet briefs, `HOBBES_DECISIONS`, and dispatch defaulting to the agent lane.
3. **Agent time, guarded.** Grant, scope, declared intent, full log, proposals only, and the trust ledger feeding suggestions. Off by default. Ships only when the log and pause controls are solid.
4. **Weekly review and measures.** The review screen, and leverage alongside ground covered.

## 7. Open questions

- Does agent time draw from the same model and connector as the user's chats, or from a cheaper one chosen in the grant?
- ~~Should your-time categories be shared between people in one household?~~ **Decided (2026-10-07): Hobbes stays a personal planner for now.** Household sharing implies a cloud version of Hobbes with SSO (accounts, invites, shared vs private items, conflicts). That is a future-phase direction, not part of these phases. If any household view ships earlier, other people are simple local profiles (a name and a colour, no accounts or permissions) that could later map to SSO identities.
- How do we show "getting outside", the category that keeps losing, so it gets protected first rather than last?
- Where does agent time's output live long-term: Notes, a Proposals inbox, or the to-do list?
- What is the smallest scope widening worth suggesting first? Probably "file accepted research digests without asking".
