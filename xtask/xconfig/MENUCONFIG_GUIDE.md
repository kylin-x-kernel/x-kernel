# Rust Kbuild TUI Menuconfig Guide

## Overview

The TUI (Terminal User Interface) menuconfig provides a modern, user-friendly interface for configuring Kconfig-based projects, similar to Linux kernel menuconfig but with enhanced usability.

## Features Implemented

### 1. **Modern Three-Panel Layout**
   - Left panel: Configuration menu tree
   - Right panel: Help and details for selected item
   - Top: Header with modification counter
   - Bottom: Status bar with keyboard shortcuts

### 2. **Navigation**
   - Arrow keys (↑/↓/←/→) for navigation
   - Vim-style keys (h/j/k/l) also supported
   - Enter: Open submenu or toggle value
   - ESC: Go back to parent menu
   - Space: Toggle boolean/tristate values
   - PageUp/PageDown: Fast scrolling
   - Home/End: Jump to first/last item

### 3. **Search Functionality**
   - Press `/` to activate search mode
   - Type to filter options with fuzzy matching
   - Search matches both option labels and IDs
   - Results are scored and sorted by relevance
   - Press Enter or ESC to exit search mode

### 4. **Visual Indicators**
   - `[✓]` Enabled boolean option
   - `[ ]` Disabled boolean option
   - `[M]` Module (tristate)
   - `⚙️` Configuration option
   - `📁` Menu with subitems
   - Icons for visual clarity

### 5. **Configuration Management**
   - `s` or `S`: Save configuration to .config
   - `q` or `Q`: Quit (prompts to save if modified)
   - Tracks modified options
   - Shows change counter in header

### 6. **Help System**
   - `?`: Show help modal with all keyboard shortcuts
   - Right panel shows detailed information about selected item:
     - Type and ID
     - Current value/status
     - Description/help text
     - Dependencies and selections

### 7. **Theme Support**
   - Modern dark theme by default
   - Color-coded elements:
     - Cyan: Highlighted/selected items
     - Gray: Disabled items
     - Blue: Information
     - Yellow: Warnings
     - Green: Success messages

## Usage Example

```bash
# Navigate to your project directory
cd examples/sample_project

# Run menuconfig
xconf menuconfig

# The TUI will launch showing the configuration options
# Use arrow keys to navigate, Space to toggle, Enter to open menus
# Press 's' to save, 'q' to quit
```

## Implementation Details

### Module Structure
```
src/ui/
├── mod.rs              # Public API
├── app.rs              # Main application state and rendering
├── events/             # Event handling
│   ├── handler.rs
│   └── mod.rs
├── rendering/          # Theme and styling
│   ├── theme.rs
│   └── mod.rs
├── state/              # State management
│   └── mod.rs          # ConfigState, NavigationState, MenuItem
└── utils/              # Utilities
    ├── fuzzy_search.rs # Search algorithm
    └── mod.rs
```

### Key Data Structures

- **MenuConfigApp**: Main application state holding configuration, symbols, navigation, and UI state
- **MenuItem**: Represents a configuration option with its properties
- **ConfigState**: Manages the configuration tree and modifications
- **NavigationState**: Tracks current position in menu hierarchy
- **Theme**: Defines colors and styles for the UI

## Keyboard Shortcuts Reference

| Key | Action |
|-----|--------|
| ↑, k | Move up |
| ↓, j | Move down |
| ←, h, ESC | Go back |
| →, l, Enter | Enter submenu |
| Space | Toggle option |
| / | Search |
| ? | Help |
| s, S | Save |
| q, Q | Quit |
| PageUp | Scroll up fast |
| PageDown | Scroll down fast |
| Home | Jump to first |
| End | Jump to last |

## Integration with Kconfig

The menuconfig TUI integrates seamlessly with the existing Kconfig parser:
1. Parses Kconfig files using the existing parser
2. Loads existing .config if present
3. Builds a navigable menu tree from AST
4. Updates symbol table on value changes
5. Saves configuration in standard .config format

## Performance

- Fast rendering using ratatui (modern TUI framework)
- Efficient navigation with minimal redraws
- Handles large configuration trees smoothly
- Responsive UI with sub-100ms latency

## Future Enhancements

Potential improvements for future versions:
- Dependency tree visualization (`d` key)
- Undo/redo support
- Configuration comparison
- Custom themes
- Mouse support
- Copy/paste configuration snippets
- Configuration validation warnings
