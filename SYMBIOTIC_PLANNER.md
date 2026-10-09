# Symbiotic Planner — Human Time, Agent Work, Agent Time

Status: vision + design direction (2026-10-07); agent time v2 (2026-10-09), experimental. Design canvas for agent time: https://claude.ai/artifact/L7mzLyrYMZeEeknx1YZZgx (board "Agent time · the review when you're back").
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

The direction here is a third lane: **agent time**. It is a small, human-granted window in which the harness works on its own choice of work, done **in the open**. The aim is to build trust step by step, so humans and models (and the rest of life) move ahead together rather than apart.

### Why v2

The first version handed every output back as a proposal, and accepting one opened a new chat tab. That was delegation with extra steps: the person still started each piece of work and finished it. Agent time is worth having only if the work finishes without the person, and Hobbes picks up work the person would not have thought to hand out.

So v2 runs in a **window**, either scheduled (overnight, say) or started by hand. A window's work happens inside one agent-time chat and ends in a single **review** the person reads later. It is experimental, and it stays that way until the review has been used for a few weeks.

### Four jobs

A window runs up to four jobs. Budget left over goes to Tend.

- **Prepare.** Briefs, drafts and background for commitments the person flagged "Prep this". Never other tasks.
- **Finish.** Works the agent-lane queue. Results are **deliverables**, graded in the review as Keep, Needs work or Discard. They are not new tabs.
- **Notice.** Proposals for changes to the person's own stuff: a to-do that looks stale, a duplicate, a deadline that moved. Each is accepted or dismissed.
- **Tend.** Hobbes's own upkeep. It reads freely but writes only to Hobbes-owned space: corrections to things it told the person, fixes to its own friction, notes on what connected tools can actually do, and tidying its memory. Tend fills whatever budget is idle.

**Corrections come first in Tend.** If they regularly catch real mistakes, that alone justifies agent time. If they stay empty for weeks, the experiment has said something too.

### Asking without stopping

A window never halts to ask. When it needs a decision, it saves a **question** with its options and moves on. The person answers in the review, and the answers reach the next window.

### The review

The review is titled **While you were away**. It has:

- a header with the window, the minutes and spend used, a link to the full run log, and **Mark reviewed**
- **Saved for you**: the questions
- one section per job: Prepare, Finish, Notice, Tend
- a **trust-ledger strip**: graded so far, corrections caught, out-of-scope attempts

Each result card has a chat icon in its upper-right corner. It starts a message about that result in a **chat rail** beside the review. The rail is the agent-time chat, and "Open as tab" turns it into an ordinary tab. Comments stay with the run and shape the next window. A reply spends agent time only when the person asks for more work.

### The rules of agent time

- **Granted, not taken.** The person sets the grant: minutes per window, a spend cap, which days, and scope. Zero is a valid setting. It is off by default.
- **Scoped.** Agent time reads freely within what the person has shared, but **writes only to Hobbes-owned space**. Anything with an outside effect (sending, purchasing, publishing, changing a repo, running a command, looking up a person) goes through the normal approval gates, exactly as it does for any other work.
- **Declared up front.** Before a window starts, the agent states its intent for each job it plans to run, in one or two lines.
- **Logged in full.** Every tool call is logged: what was called, with what, what came back, and what it cost. The full run log is one tap from the review. Nothing happens off the record.
- **Interruptible.** The person can pause, stop or narrow a window at any moment, and a pause always takes effect immediately.
- **Graded, not auto-accepted.** Finished work lands as deliverables the person grades Keep, Needs work or Discard. Notices are accepted or dismissed. Both are trust signals.
- **Your tasks are yours.** Prepare works only on commitments flagged "Prep this". Finish works only on agent-lane to-dos (queued or stuck, with no worker attached). Human-lane to-dos may be read for context, but agent time never acts on them. For those it may only **offer**: a Notice reading "Hobbes offers to take: …". *(Learned in the first live run: agent time picked up a personal call Dustin was preparing and started researching the person involved.)*
- **Accepting an offer queues the task** for the next window, or hands it to a run already in progress. It does not open a tab.

### Trust ledger

Agent time feeds the trust model Hobbes already has (user-written trust rules, plus gate-log evidence that only *proposes* promotions).

- **Grading is the main evidence.** Each Keep, each accepted Notice and each kept promise ("I'll have this ready by Friday") adds to the ledger. Each Discard, dismissed Notice or broken promise counts against it.
- **Past verdicts can be revisited.** The person can recount or remove a verdict, with a note. The agent sees that note in the next window.
- Evidence never grants anything by itself. The ledger can only **suggest** widening scope or budget ("the last 20 research digests were all kept, allow agent time to file them straight into Notes?"). The person decides.
- Out-of-scope attempts, broken promises and discards narrow suggestions automatically, and are shown plainly.

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
  - `AgentTimeGrant { window_minutes, spend_cap, days, scope, schedule }`. `schedule` is a time of day, or manual only.
  - `AgentTimeWindow { trigger: Scheduled|Manual, started_at, ended_at, minutes_used, spend, jobs, run_log }`. Every tool call lands in `run_log`.
  - `AgentTimeResult { job: Prepare|Finish|Notice|Tend, body, grade: Unreviewed|Keep|NeedsWork|Discard, linked_todo }`. Deliverables and Notices both use this.
  - `AgentTimeQuestion { body, options, answer: Option<String> }`. Answers are read at the start of the next window.
  - `AgentTimeNote { result_id, body }`. Chat-rail comments, kept with the run.
  - `TrustVerdict` rows, so a past grade can be recounted or removed, with a note.

### AI tools (`src/mcp/planner_client.rs`, handlers in `src/todo/handlers.rs`)

- `HOBBES_TODO_CREATE` / `UPDATE` gain `lane`, `kind`, `outcome`. When an outcome is clear and no human presence is needed, the model should suggest the **agent** lane, not choose it.
- `HOBBES_PLAN_DAY`:
  - plans the human lane around protected your-time blocks and the weekly goals
  - batches chores into one or two slots
  - lists the agent lane separately: what to dispatch, what is working, what needs a decision
- `HOBBES_DISPATCH` becomes the natural way to start an agent to-do. It sets `lane = Agent` and `agent_state = Queued → Working`.
- **New `HOBBES_DECISIONS`** — the decision queue. It lists agent to-dos in `NeedsYou` with their one-line question, options and estimated time, and resolves one, which hands the answer back to the worker. This is the "decision queue with re-entry briefs" from the chief-of-staff direction.
- **New `HOBBES_TIME_GOALS`** — read and update your-time goals and this week's progress.
- **New `HOBBES_AGENT_TIME`** — used only by the harness during a granted window:
  - `start(window)` checks the grant and budget and opens the window and its one agent-time chat
  - `declare(job, intent)` states the intent for a job before it runs
  - `deliver(job, body)` files a Finish or Prepare result; `notice(body)` files a Notice or an offer
  - `ask(body, options)` saves a question for the review instead of stopping
  - `end(summary)` closes the window and writes the review

  It refuses outside a granted window. It is never available in normal chat.
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
4. **Agent time panel** — the grant (window minutes, cap, days, scope, schedule), Start now, the live run log, Pause/Stop, and a link to the latest review. The review itself is the screen described in section 3: Saved for you, the four job sections, the trust-ledger strip, and the chat rail.
5. **Weekly review** — your time by category against goals; agent leverage (agent minutes per engaged minute, building on the existing exocortex figure); judgment calls against routing calls; agent-time deliverables graded Keep.
6. **Tidy tabs.** Tabs are workspaces for to-dos and agent work, so they pile up as work moves async. A tidy pass suggests changes and you review them:
   - close tabs whose linked to-do is done
   - close tabs idle 7+ days with no open to-do
   - file unsorted tabs into a project, using the existing `TabGroupBy::Project` grouping

   Closed tabs stay in History; nothing is deleted. It's **manual by default**: a broom button in the sidebar header, or a step in the end-of-day ritual. An optional nudge comes when more than 20 tabs are open.
7. **Tab navigation follows what you see.** Today `next_tab` / `prev_tab` (⇧⌘] / ⇧⌘[) cycle the top strip's open order (chats, then Planner, then Fleet; `cycle_tab_fn` in `main.rs`). In sidebar mode the list is sorted and grouped (`tab_list::arrange_tabs`), so the shortcuts jump around. The change:
   - ⇧⌘] / ⇧⌘[ cycle the sidebar's arranged order, with Planner and Fleet first and collapsed groups skipped
   - new ⌥⇧⌘] / ⌥⇧⌘[ jump to the next or previous group
   - ⌘1–9 count positions in the sidebar

   This is a small fix that can ship before the rest.

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
- **Trust**: the Keep rate on graded deliverables, corrections caught by Tend, in-scope rate, and kept promises.

## 6. Phasing

1. **Lanes and your time.** `lane` / `kind` and `TimeGoal`, protected your-time blocks, the two-lane Today view, capture syntax, and `HOBBES_TIME_GOALS`. No new autonomy. Immediately useful for the human side.
2. **Agent lane and the decision queue.** `outcome` / `agent_state` / `decision` driven by fleet briefs, `HOBBES_DECISIONS`, and dispatch defaulting to the agent lane.
3. **Agent time v2, guarded.** Scheduled or manual windows running the four jobs, saved questions, one review with grading, the chat rail, and the trust ledger feeding suggestions. Off by default. Ships only when the run log and pause controls are solid.
4. **Weekly review and measures.** The review screen, and leverage alongside ground covered.

## 7. Open questions

- Does agent time draw from the same model and connector as the user's chats, or from a cheaper one chosen in the grant?
- ~~Should your-time categories be shared between people in one household?~~ **Decided (2026-10-07): Hobbes stays a personal planner for now.** Household sharing implies a cloud version of Hobbes with SSO (accounts, invites, shared vs private items, conflicts). That is a future-phase direction, not part of these phases. If any household view ships earlier, other people are simple local profiles (a name and a colour, no accounts or permissions) that could later map to SSO identities.
- How do we show "getting outside", the category that keeps losing, so it gets protected first rather than last?
- How long do graded deliverables and run logs stay in the review before they are archived? Keeping them forever helps the trust ledger; keeping them too long clutters it.
- Does Tend need its own slice of budget, so that Finish work never starves the corrections?
- What is the smallest scope widening worth suggesting first? Probably "file kept research digests without asking".
