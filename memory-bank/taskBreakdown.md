# Task Breakdown: Hobbes MVP

This document provides a detailed breakdown of all tasks required for the MVP.

| ID | Task | Assigned Mode | Dependencies | Status | Milestone |
|---|---|---|---|---|---|
| T1 | **Project Setup:** Initialize a new Dioxus project. | `code` | - | Complete | M1 |
| T2.1 | **Basic Window:** Launch a simple, visible, empty window. | `code` | T1 | Complete | M1 |
| T2.2 | **Hide Window on Launch:** Default the window to be hidden. | `code` | T2.1 | Complete | M1 |
| T2.3 | **Basic Tray Icon:** Add a system tray icon. | `code` | T2.1 | Complete | M1 |
| T2.4 | **Tray Menu with Quit:** Add a functional "Quit" button to the tray. | `code` | T2.3 | Complete | M1 |
| T2.5 | **Toggle Window from Tray:** Add a "Toggle Window" option to the tray. | `code` | T2.2, T2.4 | Not Started | M1 |
| T3 | **Chat UI:** Develop the chat interface. | `code` | T2.5 | Not Started | M2 |
| T4 | **Local Storage Service:** Implement chat history persistence. | `code` | T1 | Not Started | M2 |
| T5 | **LLM Service:** Implement API communication. | `code` | T1 | Not Started | M2 |
| T6 | **UI Integration:** Connect UI to Local Storage and LLM services. | `code` | T3, T4, T5 | Not Started | M2 |
| T7 | **Native Hotkey Manager:** Implement global hotkey registration. | `code` | T2 | Not Started | M3 |
| T8 | **Native Context Service:** Implement active window capture. | `code` | T2 | Not Started | M3 |
| T9 | **Final Integration & Testing:** Perform end-to-end testing. | `code`, `test` | T6, T7, T8 | Not Started | M4 |