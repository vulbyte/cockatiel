# THE NEW TUI FOR COCKATIEL

note: all current sub windows should be migrated to this new structure.

Architecture & Engineering Specification: Blender-Inspired TUI Layout Engine
1. Core Architecture Spec: The BSP Layout Engine
The layout engine treats the terminal terminal display screen as a single root rectangle that is recursively divided into subregions using a Binary Space Partitioning (BSP) structural tree.
1.1 Data Structures
            [ Split Node ] (Axis: Vertical, Ratio: 0.40)
             /          \
     [ Leaf Node ]     [ Split Node ] (Axis: Horizontal, Ratio: 0.60)
     ID: "pane_1"       /          \
                [ Leaf Node ]    [ Leaf Node ]
                ID: "pane_2"     ID: "pane_3"
• Node (Abstract/Variant): Can be either a SplitNode or a LeafNode.
• SplitNode:
	• axis: Enum (HORIZONTAL, VERTICAL)
	• ratio: Float range (0.05, 0.95). Defines the percentage of space allocated to the left/top child.
	• child_left / child_top: Pointer to a Node.
	• child_right / child_bottom: Pointer to a Node.
• LeafNode (The Subwindow Window Frame):
	• id: Universally Unique Identifier (UUID or auto-increment integer).
	• view_type: String/Enum representing the current application tool module loading inside it (e.g., "terminal", "file_manager", "log_viewer").
	• state: Object containing the focused layout, scroll tracking, and specific widget states.
	• calculated_rect: Evaluated structural bounding box containing absolute system cursor coordinates (x, y, width, height).
1.2 Layout Calculation Flow (The Compute Phase)
Every time the terminal screen sizes change, or a border is modified, a recursive recalculation pass runs starting from the root node down to the leaf node:
Function ComputeLayout(Node, TargetRect):
    Node.calculated_rect = TargetRect
    If Node is LeafNode:
        RenderLeaf(Node, TargetRect)
        Return
        
    If Node is SplitNode:
        If Node.axis == VERTICAL:
            LeftWidth = Math.Round(TargetRect.width * Node.ratio)
            RightWidth = TargetRect.width - LeftWidth
            
            LeftRect = Rect(TargetRect.x, TargetRect.y, LeftWidth, TargetRect.height)
            RightRect = Rect(TargetRect.x + LeftWidth, TargetRect.y, RightWidth, TargetRect.height)
            
            ComputeLayout(Node.child_left, LeftRect)
            ComputeLayout(Node.child_right, RightRect)
            
        If Node.axis == HORIZONTAL:
            TopHeight = Math.Round(TargetRect.height * Node.ratio)
            BottomHeight = TargetRect.height - TopHeight
            
            TopRect = Rect(TargetRect.x, TargetRect.y, TargetRect.width, TopHeight)
            BottomRect = Rect(TargetRect.x, TargetRect.y + TopHeight, TargetRect.width, BottomHeight)
            
            ComputeLayout(Node.child_top, TopRect)
            ComputeLayout(Node.child_bottom, BottomRect)
2. Structural Layout Edge Cases & Rules
To maintain layout integrity and replicate Blender's window rules, the engine enforces strict structural properties during mutation requests.
2.1 The "Join / Collapse" Rule (The Alignment Rule)
You cannot join arbitrary adjacent windows. Two windows can only merge if they are direct siblings in the BSP tree, or if their combined edge creates a clean bounding box without overlapping partial regions.
• The Algorithmic Constraint: A merge event is valid only if the boundary edge between Leaf A and Leaf B spans exactly 100% of their touching alignment axis.
• Execution: When Leaf A swallows Leaf B, the parent SplitNode is removed from the tree, and the remaining sibling is promoted up to take the parent's structural spot.
2.2 Terminal Space Constraints (Min-Width / Min-Height Protection)
• The Constraint: To prevent rendering crashes, no window cell can be smaller than a boundary threshold: MIN_WIDTH = 12 columns, MIN_HEIGHT = 5 rows.
• Resolution: Before completing a user window split or border adjustment, calculate the resulting dimensions. If either child falls below the threshold, the mutation command is aborted and flashes an error notification state.
3. UI Component Spec: Upper Left Corner (Header Context Selector)
Every LeafNode panel renders a top header frame containing global navigation hooks.
+---------------+------------------------------------------+

| [v] Log View  | File   Edit   Options                    |
+---------------+------------------------------------------+

| 2026-09-30 18:31:02 - INFO - Engine initiated...        |
|                                                          |
+----------------------------------------------------------+
3.1 Dropdown Action Architecture
• The Visual Element: The header begins with an interactive prompt string element like [v] Editor Type.
• The Activation Flow: Clicking the token or executing the context shortcut (Ctrl + T) overlays a temporary float modal selector box listing registered application components.
• Dynamic Morphing: Selecting a new choice instantly changes the view_type property inside that specific node, destroys the previous widget cache, and mounts the new widget frame inside the calculated bounding box dimensions without restarting the layout loop.
4. Keymap Configuration & Re-binding Specification
The engine detaches keyboard string reads from execution commands through an action registry layer.
4.1 Keymap Definition Object
The entire key map configuration is saved and structured in a serializeable layout format (JSON or TOML) allowing quick user override declarations.
json
{
  "global_context": {
    "focus_next": "Tab",
    "focus_prev": "Shift+Tab",
    "open_tts_help": "F1"
  },
  "window_management": {
    "split_vertical": "Ctrl+B v",
    "split_horizontal": "Ctrl+B h",
    "close_pane": "Ctrl+B x",
    "change_editor_type": "Ctrl+T",
    "navigate_left": "Ctrl+ArrowLeft",
    "navigate_right": "Ctrl+ArrowRight",
    "navigate_up": "Ctrl+ArrowUp",
    "navigate_down": "Ctrl+ArrowDown"
  }
}
Use code with caution.
5. Accessibility Spec: Blind Navigation & Screen Reader (TTS) Interception
Standard terminal screens fail screen readers because they parse raw character streams left-to-right across the whole interface, creating garbled jargon when read across side-by-side vertical columns.
5.1 System Screen Reader Interception Layer
To solve this, the application captures structural layout details and funnels semantic descriptions straight to the operating system's Text-To-Speech (TTS) engine, utilizing system pipes or native hooks (e.g., speech-dispatcher on Linux, SAPI on Windows, NSSpeechSynthesizer on macOS).
5.2 Accessibility Focus Navigation Flow
When assistive-vision mode is enabled:
1. Spatial Auditory Announcements: Moving your active cursor into a window panel triggers a structural context speech overview announcement.
	• TTS Feedback Format: "Subwindow focused. Position: Top Left. Current tool: File Manager. Press F1 for tooltip actions."
2. Isolated Panel Processing: The screen reader mode isolates input focus context. The user can request the terminal to read only the contents of the isolated active block container.
5.3 TTS Escape Tooltip Core Hook
Pressing F1 (or the customized global help key) halts regular terminal input states and invokes a dedicated descriptive modal helper frame.
+-----------------------------------------------------------+

| ACCESSIBILITY HELP & ACTIONS (Press ESC to close)         |
+-----------------------------------------------------------+

| You are currently interacting with: [Log View Module].    |
| Layout Tree Depth: 2 of 4 windows active.                 |
|                                                           |
| Available Controls:                                       |
| * Press <split vertical> to split this view vertical      |
| * Press <split-horizontal> to split this view horizontally|
| * Press <close-binding> to close this workspace           |
+-----------------------------------------------------------+
[User Presses Escape Key]
       │
       ▼
[Close TTS Tooltip Overlay] ──► [Return Focus To Isolated Subwindow]
                                 └──► TTS Speaks: "Resuming Log View context."

• The Escape Safe Route: Pressing Escape while the help tooltip or choice dropdown context is active guarantees a complete rollback to the prior safe structural focus state. It clears out open prompt lists and speaks an auditory confirmation ("Context closed, returning to editor.") ensuring users never lose track of their cursor orientation inside complex terminal split views.
Would you like to build a concrete Python rendering prototype of the structural BSP window splitting algorithm next, or should we refine the data structures for edge-case tracking?

---

example of the new UI (note, the windows are empty because whatever is drawn in those windows is handled by the logic for those sub windows, what we're seeing right now is the constants). the text at the bottom is for the window bindings, which can be toggled in the TUI config.json. the things within <> are keybinds which should exist within a keybinds file that users can change the mappings of, and the content within "[]" is the name of the module.:
┌───────────────────────────────────────────┐──────────────────────────────────┐
│[cockatiel_info]   │[top users]            │[module_manager]                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│                   │                       │                                  │
│───────────────────────────────────────────│                                  │
│[engine_graph]                             │                                  │
│                                           │                                  │
│                                           │                                  │
│                                           │                                  │
│                                           │──────────────────────────────────│
│                                           │[logs]                            │
│                                           │                                  │
│                                           │                                  │
│                                           │                                  │
└───────────────────────────────────────────┘──────────────────────────────────┘
v: xxx.yyy.zzz | split-h:<split-h> split-v:<split-v> split-c:<split-c> quit:<qui

---

to change the content of a window, the user should be able to select it from a drop down, when tabbing or clicking on the [subwindow-name] a drop down should appear exactly 1 line below the sub-window name. 
if the dropdown would go off screen, the it should be shifted to the left until it fits and is completely visable; if is too narrow to properly display, then display normally.
it looks like the following (is dynamic to the length of the names and the width of the names).
note: the ">" indicates the currenty selected module. it should also be highlighted using a background color. upon selection or close it should close the popup. the help indicators for select and exit should not be selectable.

[current_subwindow]    
┌───────────────────┐  
│ module_1          │  
│ current_subwindow │  
│>module_n          │  
│ module_n          │  
│ module_n          │  
│ module_n          │  
│                   │  
│select:  <confirm> │  
│exit:    <deny>    │  
└───────────────────┘  

---
