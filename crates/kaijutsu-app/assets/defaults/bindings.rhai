// Kaijutsu default key bindings
//
// This file is loaded from ~/.config/kaijutsu/bindings.rhai on startup
// and hot-reloaded when changed (checked every 2 seconds).
//
// ## API
//
//   binding(key, context, action, label)           // simple key binding
//   binding_mod(key, mods, context, action, label) // key + modifiers
//   gamepad(button, context, action, label)        // gamepad button
//   default_bindings()                             // returns this array
//
// ## Contexts
//
//   "Global"        — always active: F1/F12, Alt+pane keys
//   "Navigation"    — conversation block list focused: j/k, i/Space, :
//   "TextInput"     — compose area: Enter, Backspace, arrows, Ctrl+Z/C/V
//   "Constellation" — constellation screen: Tab, Esc, Enter, f, n, m
//   "Dialog"        — modal dialogs: Enter, Esc, j/k, Tab
//
// ## Modifiers
//
//   ""            — no modifiers
//   "CTRL"        — Ctrl held
//   "SHIFT"       — Shift held
//   "ALT"         — Alt held
//   "CTRL+SHIFT"  — Ctrl+Shift held
//
// ## Customization example
//
//   let b = default_bindings();
//   // Remove a binding (e.g., Quit on 'q' in Navigation)
//   b = b.filter(|x| !(x["key"] == "KeyQ" && x["context"] == "Navigation"));
//   // Add a new binding
//   b.push(binding("KeyQ", "Navigation", "Quit", "Quit"));
//   b

// Return the default binding list.
default_bindings()
