//! The Heartbeat: a harness that stays awake between chats (Phase 2).
//!
//! Perception (cheap watchers, no LLM) turns planner, Fleet and CI changes
//! into events; salience routes each to ignore / note / queue / wake; the
//! budget caps wakes; a wake hands the dedicated Heartbeat session a
//! briefing. Design: the "Hobbes Heartbeat" doc.

pub mod ci;
