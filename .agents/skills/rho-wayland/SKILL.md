---
name: rho-wayland
description: Launch and control GUI applications in rho's isolated headless Wayland compositor, including screenshots, clicks, typing, and key presses. Use for GUI testing, visual QA, or any task that needs an agent to interact with a native Wayland application.
---

# Headless Wayland control

Use `rho wayland` to run native GUI applications in an isolated headless Sway
session.

## Start a session

Start the application as trailing arguments after `--`:

```bash
rho wayland --session gui start -- APPLICATION ARG...
```

The default output matches a 13-inch MacBook display: 2560×1664 physical
pixels at 2× scale (1280×832 logical pixels). Override `--width`, `--height`,
or `--scale` when a test needs different geometry.

For `rho-gui`, preserve the daemon socket path before the driver gives the GUI
its private `XDG_RUNTIME_DIR`, and pass the socket explicitly:

```bash
rho_socket="$XDG_RUNTIME_DIR/rho/rho.sock"
rho wayland --session gui start -- rho-gui --socket "$rho_socket"
```

Use distinct session names when operating more than one application. A session
continues after `start` returns. Always stop it when finished.

## Observe and interact

Wait for application state rather than taking a screenshot immediately. Use
`status` to check process liveness and `tree` for Sway's JSON window tree:

```bash
rho wayland --session gui status
rho wayland --session gui tree
sleep 2
rho wayland --session gui screenshot --output /tmp/gui.png
```

Interact using output pixel coordinates:

```bash
rho wayland --session gui move 500 300
rho wayland --session gui click 500 300
rho wayland --session gui click 500 300 --button right
rho wayland --session gui type 'literal text'
rho wayland --session gui key ctrl+enter
```

Supported key modifiers are `ctrl`, `alt`, `shift`, and `super`. Common key
names such as `enter`, `escape`, `tab`, arrows, `home`, `end`, `pageup`, and
`pagedown` are normalized for `wtype`.

After every input that changes the interface, wait briefly or check observable
state, then take another screenshot. Do not assume a click succeeded solely
because the command exited successfully.

## Clean up

```bash
rho wayland --session gui stop
```

The session sockets are private but applications are not sandboxed: launched
programs retain the invoking user's authority. Do not point a test GUI at a
production daemon unless the task specifically requires interacting with it.
