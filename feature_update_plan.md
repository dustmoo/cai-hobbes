# Plan: Dioxus Tray Icon Update and Refinement

This document outlines the plan for updating and refining the Dioxus tray icon implementation.

## 1. Dependency Alignment

The most critical step is to ensure the application's dependencies are consistent. Since the application is using Dioxus v0.6, we will proceed with that version.

## 2. Icon Asset Refinement

*   **Convert to PNG:** The existing `favicon.ico` will be converted to a PNG format to improve cross-platform compatibility.
*   **Asset Loading:** The `include_bytes!` macro will continue to be used to embed the icon in the binary.

## 3. Tray Icon Logic Enhancements

*   **Dynamic Menu:** The tray icon menu will be modified to be more dynamic. This will involve creating a state that can be updated to change the menu items at runtime.
*   **Toggle Window Visibility:** A menu item will be added to toggle the visibility of the main application window.

## 4. Code Organization

*   **Tray Icon Module:** A dedicated module (e.g., `tray.rs`) will be created to house all the tray icon-related logic, improving code organization and maintainability.

## Visualization

Here is a Mermaid diagram illustrating the proposed changes:

```mermaid
graph TD
    A[main.rs] --> B{Initialize Tray Icon};
    B --> C{Load Icon Asset};
    B --> D{Create Tray Menu};
    D --> E[Menu Item: Toggle Window];
    D --> F[Menu Item: Quit];

    subgraph "tray.rs (New Module)"
        direction LR
        C -- Manages --> G((icon.png));
        D -- Manages --> H((Menu State));
    end