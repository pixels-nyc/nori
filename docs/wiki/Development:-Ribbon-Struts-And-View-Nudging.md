# Development: Ribbon Struts & Non-Destructive View Nudging

> **Target**: `nori` Display Server (Smithay-based Niri fork)  
> **Topic**: Preventing client window resize when edge layer-shell surfaces or struts appear.

---

## 1. Context & Motivation

A core principle of `niri` / `nori` is that:
> **"Opening a new window should not affect the sizes of any existing windows."**

However, in upstream `niri`, when a layer-shell surface (such as a sidebar or panel dock) claims an exclusive zone along a vertical edge (`Edge::LEFT` or `Edge::RIGHT`):
1. `Smithay` reduces `non_exclusive_zone()`.
2. `compute_working_area()` shrinks `working_area.size.w`.
3. Proportional columns (`ColumnWidth::Proportion(1.0)`, `ColumnWidth::Proportion(0.5)`) recalculate their width against `working_area.size.w`.
4. As a result, **every open window is forcibly resized**, causing terminal re-wrapping, browser DOM reflow, and visual squishing.

In a scrollable-tiling compositor with an infinite horizontal ribbon, **windows do not need to shrink**. There is limitless space to the right of the visible screen.

---

## 2. nori Feature Specification

### 2.1 The Nudge-Ribbon Strut Model
Instead of compressing the column widths:
- The monitor's logical layout width remains constant (`output_width`).
- Proportional columns resolve against stable output bounds.
- The **horizontal viewport offset** (`view_offset` in `ScrollingSpace`) simply shifts/nudges to the right by the strut width (e.g., +52px for a sidebar rail).
- The column slides smoothly across the screen, docking cleanly next to the sidebar.
- Its right edge extends naturally into the horizontal scroll area off-screen.

### 2.2 Configuration Syntax (`config.kdl`)
```kdl
layout {
    // "nudge-ribbon" (default in nori) | "squash-columns" (legacy upstream niri)
    strut-behavior "nudge-ribbon"

    strut-nudge-animation {
        spring damping-ratio=0.88 stiffness=800 epsilon=0.001
    }
}
```

### 2.3 IPC Action
```bash
# Programmatically shift viewport offset without mutating client surface bounds
nori msg action shift-view-offset 52
```

### 2.4 Implementation Roadmap in `src/layout/scrolling.rs`
1. Decouple `working_area.size.w` from left/right exclusive zones when `strut-behavior == "nudge-ribbon"`.
2. Update `compute_new_view_offset` to anchor the left-aligned column at `ribbon_offset_left` instead of hardcoded `0.0`.
3. Animate the transition using existing spring animations in `nori`.
