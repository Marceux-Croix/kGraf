# Stage UI Style Guide

Version 1.10. This guide stands alone. It depends on no project. Copy it into a
project and apply it as written.

Stage UI is built for interfaces that center on one large canvas, the stage, such as a graph, a
map or a timeline. The interface keeps attention on the stage, and it offers information density
that scales to support what's displayed on the stage.

Think of a dark theater just before the show: the house lights fall, the stage glows, and
everything else waits in the wings until it is called. The work is the performance, and the
interface is the stagehand: quiet, precise, and always there when it is needed.

Its palette is called Nocturne: blue-black surfaces, with violet, cyan, yellow and orange as the only signal colors.

## 1. Principles

These seven principles decide any case that the rules do not cover.

1. **The stage is the product.** The main content sits on a black stage. The page frames the stage
   and never competes with it. The stage stays black in every theme.
2. **Chrome stays in the background.** Controls on the stage show when the user is active and hide
   when the user is idle. The content is always the largest and highest-contrast thing in view.
3. **One meaning per color.** Violet is progress and live. Orange is broken, failed or dangerous.
   Yellow is "needs your input". Cyan is your selection, a match, or the primary action. Gray is
   everything else. No color names a category.
4. **Flat surfaces, set apart by tone.** Translucent fills and text weight show what is more
   important, not borders and shadows. Only floating layers cast a shadow.
5. **Each kind of element has a fixed shape.** Buttons are pills. Chips are rectangles with 8 px
   rounded corners. Containers are rectangles with 12 px rounded corners. Avatars and icon buttons
   are circles.
6. **Reasons are words.** A graphic shows a state. Text next to it gives the reason for the state.
   Do not invent an icon or a color for a reason.
7. **Reduced modes are visible, and actions can be undone.** When the app runs in a degraded mode
   (a mode with reduced function), a badge says so. An action with significant consequences asks
   for confirmation in place. A completed action offers Undo.

## 2. How to use this guide

- The words MUST, MUST NOT, SHOULD and MAY have the meanings defined in RFC 2119.
- Each rule has an ID (for example `COL-3`). Use the ID in reviews and commits.
- A blocking rule is one that stops a release when it is broken. Section 16 lists them.
- Section 17 has the complete token block, ready to paste. Section 18 is the checklist for
  adopting the guide.
- Words in this guide:
  - **Page**: the application window and everything in it.
  - **Stage**: the one large canvas that holds the main content (a graph, a map, a media view, a
    timeline). A layout has exactly one stage.
  - **Chrome**: the controls that float on top of the stage.
  - **Masthead**: the bar across the top of the window.
  - **Surface**: a filled area that holds content (the page background, a panel, a menu, a card).
  - **Additive fill**: a see-through white layer (dark theme) or black layer (light theme) over the
    surface below it. It makes that surface a little lighter or darker.
  - **Tonal**: uses the additive fill. A tonal button is a button with an additive fill.
  - **Inverted fill**: a fill in the text color, so it is light in the dark theme and dark in the
    light theme. Text on it uses the page color.
  - **Live edge**: the present moment. The view is at the live edge when it shows now, and
    behind live when it shows a past time.
  - **Lockup**: a thumbnail grouped with its title and metadata (CMP-ROW-6).
  - **Composer**: the text field where the user writes a message (section 9.12).

### Reading the values

Rules keep some short forms so that the values stay compact.

- **Type.** "500 14/20" means font weight 500, 14 px text and 20 px line height. Weight 400 is
  regular, 500 is medium and 700 is bold. A size with no weight, such as "12/18", gives only the
  text size and the line height. "mono" means the monospace family `--mono`.
- **Pixels.** Values are in CSS pixels. In a native toolkit, read "px" as logical points (1 px = 1
  point at scale 1.0).
- **Padding and margin.** The order is top, right, bottom, left, as in CSS. "0 4 px 0 16 px" is 0
  at the top, 4 px at the right, 0 at the bottom and 16 px at the left. Three values are top, then
  left and right, then bottom. Two values are top and bottom, then left and right: "6 px 16 px" is
  6 px above and below and 16 px at each side. A grid "gap 4 px 16 px" is 4 px between rows and 16
  px between columns.
- **Sizes.** "64 x 36 px" is width by height. A number with "x" and no unit, such as "1.4 x", is a
  multiplier.
- **Window sizes.** "100vw" is the window width and "100vh" is the window height. "70vh" is 70% of
  the window height. "min(460 px, 70vh)" means the smaller of the two values.
- **Tokens.** A name such as `--text-2` is a token: a named value from section 3 (colors), section
  4.1 (fonts) or section 8.1 (motion). Section 17 lists all of them.
- **Opacity.** "white at 50%" means white at 50% opacity. "`--primary` at 28%" means that token
  color at 28% opacity.
- **Radius** is the rounding of the corners.
- **Rule IDs in parentheses**, such as "(CMP-BDG-4)", point to another rule that also applies.

## 3. Color

### 3.1 Semantic colors

| ID | Rule |
|---|---|
| COL-1 | Violet (`--accent`, `--progress`) MUST mean only progress or live. It appears on progress bars, the timeline scrubber and the live dot. Its one other use is the single accent element of the brand mark (AST-2). |
| COL-2 | The count badge on a notification button and the live status badge use `--indicator`, a deeper violet, because white text on `--accent` has a contrast below 4.5:1. These two badges are the only other uses of violet. |
| COL-3 | Yellow (`--needs-input`) MUST mean "needs your input": a question, an approval, a decision, or a failure that the user must deal with. It means nothing else, on the stage or on the page. An indicator that sums up several items shows a failure as yellow (CMP-BDG-3). |
| COL-4 | Cyan (`--primary`) MUST mean one of these: the current selection, a search or filter match, the primary action, or "new since you last looked". On the stage, cyan means only the selection or a match. |
| COL-5 | Gray (`--text-2`, and `#9AA0B4` on the stage) is the default for all other states, including "working" and every kind of waiting (queued, blocked, held by cap, colliding). A waiting state names its reason in words. |
| COL-6 | Green (`--green`) MAY show a healthy or passed state, in text or in an 8 px status dot. Do not use green on the stage. |
| COL-7 | Orange (`--danger`, `--danger-bg` and, on the stage, `--stage-danger`) MUST mean broken, failed or dangerous. It marks the failed or broken thing itself: a failed run, check or gate, a worker that stopped with an error, a degraded service, a form error, and the confirm step before an action with significant consequences. An indicator that sums up several items never shows orange; it shows a failure as yellow. Orange means nothing else. |
| COL-8 | Color MUST NOT identify a category, such as a provider, a team or a type of user. Use a letter, a label or an icon instead. |
| COL-9 | A color MUST NOT be the only way a meaning is shown. Pair it with text, a tooltip or an accessible name (the name that a screen reader announces). |

### 3.2 Page tokens

Dark is the default theme. Each token has one value for each theme.

| Token | Dark | Light | Use |
|---|---|---|---|
| `--base` | `#0D0F17` | `#F7F8FC` | Page background, panel background |
| `--raised` | `#171A26` | `#FFFFFF` | Dropdown lists, search suggestions |
| `--menu` | `#1D2130` | `#FFFFFF` | Menus, dialogs |
| `--inverted` | `#EDEFF7` | `#10131C` | Filled buttons, active chips, snackbar |
| `--inverted-hover` | `#D3D7E4` | `#262A38` | Hover on inverted fills |
| `--additive` | `rgba(255,255,255,.1)` | `rgba(16,19,28,.05)` | Tonal buttons, chips, hover, current navigation entry |
| `--additive-soft` | `rgba(255,255,255,.05)` | `rgba(16,19,28,.03)` | Row hover in dense lists |
| `--chip-hover` | `rgba(255,255,255,.2)` | `rgba(16,19,28,.1)` | Hover on additive fills, pressed icon buttons |
| `--outline` | `rgba(255,255,255,.2)` | `rgba(16,19,28,.12)` | Dividers, panel borders, pressed tonal buttons |
| `--outline-opaque` | `#2E3345` | `#E1E4EE` | Fill of neutral avatars and tiles |
| `--text` | `#EDEFF7` | `#10131C` | Main text, icons |
| `--text-2` | `#9AA0B4` | `#5A6072` | Secondary text, metadata |
| `--text-3` | `rgba(255,255,255,.54)` | `rgba(16,19,28,.6)` | Timestamps in feeds |
| `--text-dis` | `#6B7186` | `#8A90A2` | Disabled text, and the live dot when paused or behind live |
| `--text-inv` | `#0D0F17` | `#FFFFFF` | Text on inverted and primary fills |
| `--primary` | `#22D3EE` | `#0E7490` | Primary action, selection, match |
| `--primary-hover` | `#67E8F9` | `#155E75` | Hover on primary fills |
| `--primary-inv` | `#0E7490` | `#22D3EE` | Primary color on an inverted surface (the snackbar action) |
| `--suggested` | `#10303B` | `#DDF6FB` | Highlighted row (the current row or a search hit), and a card that flashes for attention |
| `--danger` | `#FB923C` | `#B83C0B` | Broken, failed or dangerous: text, icons, dots, the error underline |
| `--danger-bg` | `rgba(251,146,60,.16)` | `#FFEDE3` | Danger badge fill |
| `--needs-input` | `#FACC15` | `#A16207` | Needs-input yellow on the page |
| `--green` | `#34D399` | `#047857` | Healthy, passed |
| `--search-bg` | `#0A0C13` | `#FFFFFF` | Search input fill |
| `--search-border` | `#2A2F40` | `#C9CEDC` | Search input border |
| `--search-btn` | `rgba(255,255,255,.06)` | `#EEF0F6` | Search button fill |
| `--search-btn-border` | `#2A2F40` | `#C9CEDC` | Search button border |
| `--search-focus` | `#0891B2` | `#0E7490` | Search input border when it has focus |
| `--tooltip` | `rgba(46,51,69,.94)` | `rgba(46,51,69,.94)` | Page tooltip fill |
| `--scrim` | `rgba(5,6,12,.6)` | `rgba(16,19,28,.4)` | Dims the page behind modal dialogs |
| `--scrollbar` | `#4A5068` | `#A3A9BA` | Scrollbar thumb |
| `--shadow-menu` | `0 4px 32px rgba(0,0,0,.35)` | `0 4px 32px rgba(16,19,28,.12)` | Menus, dialogs |

### 3.3 Fixed tokens (same in both themes)

| Token | Value | Use |
|---|---|---|
| `--accent` | `#8B5CF6` | Live dot, scrubber knob, the one accent element in the brand mark |
| `--progress` | `linear-gradient(90deg, #8B5CF6 70%, #C084FC)` | Every progress fill |
| `--indicator` | `#7C3AED` | Count badge, live status badge |
| `--stage-bg` | `#07080F` | The stage |
| `--stage-popup` | `rgba(22,25,38,.92)` | Stage tooltips, legend, popup buttons |
| `--stage-danger` | `#FF7A33` | Failed or broken items on the stage |

### 3.4 Stage palette

The stage is always near-black (`#07080F`), so its colors do not change with the theme. An edge is a line between
two nodes. A cluster disk is the round area behind a group. The text halo is a dark outline around
text that keeps it readable over any content.

| Role | Value |
|---|---|
| Background | `#07080F` |
| Group area (cluster disk) | white at 3% (1.2% when outside the highlight) |
| Edge | white at 5%, or 7.5% when the camera scale is above 1 (STG-CAM-1) |
| Node, default | `rgba(160,166,186,.58)` |
| Node, highlighted neighbor | `rgba(225,229,240,.9)` |
| Selection and match | `#22D3EE` |
| Needs input | `#FACC15` |
| Failed or broken | `#FF7A33` (`--stage-danger`) |
| Progress | `#8B5CF6` to `#C084FC` |
| Card fill | `#171A26` |
| Card border | white at 34% |
| Progress track | white at 22% |
| Agent or actor disk | `#1E2233`, border white at 60% |
| Relation arrow | `#C3C8D8` |
| Label | white at 88% (72% for third-level labels) |
| Sub-label | white at 62% |
| Group label | white at 55% (18% outside the highlight) |
| Hover ring | white at 55% to 60%, 1 px |
| Text halo | 3 px stroke in `rgba(0,0,0,.85)`, with round line joins |

## 4. Typography

### 4.1 Families

| Token | Stack | Weights |
|---|---|---|
| `--font` | `"Instrument Sans", "Helvetica Neue", Arial, sans-serif` | 400, 500, 700 |
| `--mono` | `"Fragment Mono", ui-monospace, Menlo, monospace` | 400 |

### 4.2 Scale

Each entry gives the weight, then the size / line height, in px (see "Reading the values" in
section 2).

| Role | Spec |
|---|---|
| Dialog title, brand wordmark | 700 20/28 (the wordmark has letter spacing -0.6 px and line height 1) |
| Page title (primary item under the stage), detail title | 700 18/26 |
| Stage selection title | 500 18/24 |
| Section heading, panel item name, empty-state title | 500 16/22 |
| Panel header, menu header | 400 16/22 |
| Search input, suggestion rows | 400 16/24 |
| Body, chip label, row title | 400 or 500 14/20 |
| Button label, size M | 500 14/36 (the line height equals the button height) |
| Button label, size S | 500 12/32 |
| Feed and message text, key-value text | 400 13/20 |
| Setting description, note | 400 13/18 |
| Stage popup labels (scrubber preview, stage tooltips) | 500 13/18 |
| Metadata, helper text, badges | 400 or 500 12/18 |
| Stage labels | 400 or 500 12 px |
| Feed timestamps | 400 11 px |
| Stage sub-labels, count badge on stage | 400 or 500 11 px |
| Collapsed nav labels, context tag | 400 10/14 |
| Tool output in feeds | mono 400 12/20 |
| Inline code | mono 400 0.92em (relative to the text around it), line height 1 |

### 4.3 Rules

| ID | Rule |
|---|---|
| TYP-1 | Use weight 500 for names, labels and buttons. Use 700 only for page and dialog titles, the wordmark, and the part of a search suggestion that completes what the user typed (CMP-SRCH-3). |
| TYP-2 | All numbers that change or line up (counts, times, percentages, values) MUST use tabular numerals, which give each digit the same width (`font-variant-numeric: tabular-nums`). |
| TYP-3 | Single-line names and metadata end with an ellipsis when they do not fit. A lockup title MAY show up to 2 lines before it is cut off. Long free text wraps, and it can break at any point in a word (`overflow-wrap: anywhere`). |
| TYP-4 | Text on the stage MUST have a dark halo so that it reads over any content. On the canvas, draw the text halo. In HTML overlays, use `text-shadow: 0 0 2px rgba(0,0,0,.5)`. Text over the scrubber preview uses `rgba(0,0,0,.8)`. |
| TYP-5 | Use sentence case for all labels, titles, badges and buttons: capitalize only the first word and proper names. Do not set words in all capitals. |
| TYP-6 | Inline code uses `--mono` on an `--additive` fill, with radius 4 px and padding 1 px 4 px. Inside cards, inline code does not wrap. |
| TYP-7 | Turn on font smoothing (`-webkit-font-smoothing: antialiased`). |

## 5. Spacing, shape and elevation

### 5.1 Spacing

The base unit is 4 px. Prefer these steps: 2, 4, 6, 8, 12, 16, 20, 24, 32, 40, 48. When a component rule in section 9 or 10 gives a different value (for example 3, 5, 10, 14 or 36 px), the component rule wins.

| ID | Rule |
|---|---|
| SPC-1 | Page gutters (the space along the window edges) are 16 px in the masthead and 24 px in the content columns. |
| SPC-2 | The gap between the primary column and the secondary column is 24 px. The gap between stacked panels is 12 px. |
| SPC-3 | Rows inside panels have 16 px of padding at the left and right. Dense rows have 4 to 6 px of padding at the top and bottom. |
| SPC-4 | The gap between an icon and its label is 6 px in buttons and chips, 12 px in rows with a tile, 16 px in menu items and 24 px in expanded navigation entries. |
| SPC-5 | Chips in a filter bar are 12 px apart. Chips and small buttons inside a card are 8 px apart. |

### 5.2 Radius

A pill has fully round ends. A radius of 50% makes a circle.

| Radius | Use |
|---|---|
| 2 px | Timeline segment ends, user name chip, search highlight |
| 4 px | Tooltip, small status badge, small thumbnail, duration badge, inline code, small stage card |
| 6 px | Stage card, project tile |
| 8 px | Chip, status badge in the masthead, snackbar, medium thumbnail, minimap, stage tooltip, stage preview |
| 10 px | Navigation entries, option rows |
| 12 px | Stage, panels, menus, dialogs, cards, dropdowns, notes, legend |
| Pill (at least height / 2) | Buttons, ends of the search field, segmented pill, outlined chip, count badge |
| 50% | Icon buttons, avatars, dots, scrubber knob |

### 5.3 Elevation and layers

| ID | Rule |
|---|---|
| ELV-1 | Surfaces in the layout (masthead, navigation, panels, cards) MUST NOT have a shadow. Set them apart with a different tone or a 1 px `--outline` border. |
| ELV-2 | Only floating layers cast a shadow. Menus and dialogs use `--shadow-menu`. Dropdown lists use `0 2px 4px rgba(0,0,0,.2)`. The snackbar uses `0 4px 16px rgba(0,0,0,.3)`. The switch thumb uses `0 1px 3px rgba(0,0,0,.4)`. |
| ELV-3 | Stack layers in this order (z-index, where a higher number is on top): navigation 20, masthead 30, dropdown 40, menu 45, dialog 60, snackbar 80, tooltip 100. Stage overlays stack inside the stage, from 2 to 7. |

## 6. Layout

### 6.1 App shell

The window has the masthead across the top, the navigation rail on the left, the primary column
with the stage in the center, and the secondary column of panels on the right. The secondary
column is 28% of the window width, but never less than 360 px or more than 402 px.

```
+---------------------------------------------------------------------------+
| Masthead 56 px: [menu][mark][context] [   search, max 732 px   ] [status][bell] |
+------+--------------------------------------------------+-----------------+
| Nav  | Filter chips, 52 px                              | Secondary       |
| rail |--------------------------------------------------| column          |
| 72 / | Stage (fills remaining height, radius 12)        | clamp(360 px,   |
| 240  |                                                  | 28vw, 402 px)   |
|      |--------------------------------------------------|  Panel (flex 3) |
|      | Below the stage: title row, pending cards,       |  Panel (flex 2) |
|      | composer                                         |                 |
+------+--------------------------------------------------+-----------------+
```

| ID | Rule |
|---|---|
| LAY-1 | The window does not scroll (the body is `overflow: hidden`). Each panel and each list scrolls on its own. |
| LAY-2 | Use grid rows and columns of `minmax(0, 1fr)`, so that content never pushes the layout past its space. |
| LAY-3 | The stage takes all the free height in the primary column. Its minimum height is 220 px. |
| LAY-4 | The masthead is 56 px high, with padding 0 16 px. At the start: the menu button, the brand mark and the context label (500 16/22, with a 1 px divider on its left). In the center: the search field, at most 732 px wide, with margin 0 24 px. At the end: the status controls and the notification button, 8 px apart. |
| LAY-5 | The navigation rail is 72 px wide when collapsed and 240 px wide when expanded. |
| LAY-6 | The secondary column holds at most two panels, one above the other. The upper panel shows context (the selection, search results). The lower panel is a live feed. Split the height 3:2. |
| LAY-7 | Below the stage, from top to bottom: the title row (40 px avatar, 700 18/26 title, 12/18 metadata), then the pending action cards, then the composer. |
| LAY-8 | When the window is narrower than 1360 px, show only the short form of long labels in the masthead, and reduce the search margin to 12 px. |
| LAY-9 | When the content above or below the stage changes height, fit the stage to its new space in the next frame. |

## 7. Iconography

| ID | Rule |
|---|---|
| ICO-1 | Use Material Symbols Outlined, weight 400. The default size is 24 px, and this includes the check column in menus. Use 20 px in small icon buttons and dialog tabs, 18 px in small buttons and badges, and 16 px next to 12 px text. |
| ICO-2 | Icons take the current text color (`currentColor`). Draw them as a mask or a tinted glyph, not as colored images. |
| ICO-3 | Use the outlined form at rest. Use the filled form for the active state: the current navigation entry, the trigger of an open menu, a toggle that is on. |
| ICO-4 | In a button with a label, a leading icon moves 6 px to the left, into the left padding (4 px in size S). |
| ICO-5 | An empty-state icon is 64 px with no container, or 36 px inside a 72 px `--additive` ring. |
| ICO-6 | Icons on the stage are white. Status icons inside stage cards are 13 to 18 px. |

## 8. Motion

### 8.1 Tokens

Each motion token is an easing curve: it sets how the speed changes during a transition.

| Token | Curve | Use |
|---|---|---|
| `--ease-in` | `cubic-bezier(0,0,.2,1)` | Things that enter or grow |
| `--ease-out` | `cubic-bezier(.4,0,1,1)` | Things that leave or shrink |
| `--ease-std` | `cubic-bezier(.2,0,0,1)` | Things that move or change size in place |
| `--ease-search` | `cubic-bezier(.2,0,.6,1)` | The search field as it grows |
| `--ease-spring` | `cubic-bezier(.34,1.56,.64,1)` | The attention wiggle only |

### 8.2 Durations

Where a row gives two values split by " / ", the first is for entering and the second is for
leaving.

| Change | Duration |
|---|---|
| Hover and press color | 100 ms `--ease-in` |
| Tooltip, menu, dropdown in / out | 150 ms `--ease-in` / 100 ms `--ease-out` |
| Stage chrome, overlays in / out | 250 ms `--ease-in` / 100 ms `--ease-out` |
| Dialog in / out | 250 ms: fades in (`--ease-in`) and grows from scale 0.96 to 1 (`--ease-std`) / 150 ms `--ease-out` |
| Card enter / leave | 250 ms, rising from 8 px below / 200 ms, sinking 8 px and shrinking to scale 0.98 |
| New feed row | 350 ms `--ease-std`: slides down from 8 px above, and its fill fades from `--suggested` to the normal fill |
| New message | 250 ms `--ease-in`, rising from 6 px below |
| Expand and collapse (row detail) | 300 ms `--ease-std` (grid rows 0fr to 1fr). The chevron turns 180 deg in 250 ms. |
| Field underline | 250 ms `--ease-std` |
| Snackbar | 200 ms `ease`, rising from 24 px below |
| Search field expansion | 350 ms `--ease-search` |
| Camera move to a target or fit | 500 ms `--ease-std` |
| Camera step (keys, buttons, minimap) | 200 to 250 ms |
| Progress bar value | 400 ms `--ease-std` |
| Stage event pulse | 1200 ms `--ease-in` |

### 8.3 Rules

| ID | Rule |
|---|---|
| MOT-1 | Things enter more slowly than they leave. An element that appears takes 150 to 350 ms. An element that leaves takes 100 to 200 ms. |
| MOT-2 | When the user asks for reduced motion (`prefers-reduced-motion: reduce`), all transitions and animations MUST have zero duration, blinking stops (the dot stays solid), and scrolling is instant. |
| MOT-3 | "Working" shows as a blink. The opacity goes from 1 to 0.3 and back over 1.6 s, with `ease-in-out`, and repeats without end. On the stage canvas, the lowest opacity is 0.35. The blink applies to gray working dots and to the gray aggregate indicator (CMP-BDG-4). |
| MOT-4 | The attention wiggle is only for "needs your input" on a collapsed control. Each cycle is 4.6 s: a 600 ms spring, then rest. During the spring, the control rotates +8, -6, +4 and -1 deg in turn, and scales up to 1.08. The wiggle stops on hover. A new needs-input item starts it again. |
| MOT-5 | A live event on the stage shows as one gray ring. The ring grows from 5 to 35 px and fades out over 1.2 s. It has no color. |
| MOT-6 | Do not move layout that the user is pointing at. Keep the list order stable when an item resolves (see CMP-CARD-4). |

## 9. Components

### 9.1 Buttons

Buttons come in two sizes and six variants.

| Property | Size M | Size S |
|---|---|---|
| Height | 36 px | 32 px |
| Radius | 18 px | 16 px |
| Padding | 0 16 px | 0 12 px |
| Label | 500 14 px | 500 12 px |
| Icon | 24 px | 18 px |

| Variant | Rest | Hover | Use |
|---|---|---|---|
| Primary | `--primary` fill, `--text-inv` text | `--primary-hover` | The action that completes the task: Send, Confirm |
| Filled | `--inverted` fill, `--text-inv` text | `--inverted-hover` | The recommended choice in a group of choices |
| Tonal | `--additive` fill | `--chip-hover`, pressed `--outline` | The other choices in the group |
| Text | no fill, `--text` | `--additive` | Cancel, dismiss |
| Text primary | no fill, `--primary` | primary at 15% | Done, and other minor positive actions |
| Disabled | `--additive` fill, `--text-dis` text (text variants: no fill) | none | Not available |

| ID | Rule |
|---|---|
| CMP-BTN-1 | A region has at most one primary button. |
| CMP-BTN-2 | In a group of choices, the recommended or safest option is filled. The other options are tonal. |
| CMP-BTN-3 | A busy button is disabled. It shows an 18 px spinner (a 2 px ring, open on one side, that turns once every 800 ms) and a verb that ends in -ing ("Thinking"). Set `aria-busy`. |
| CMP-BTN-4 | Labels are verbs or verb phrases, with no punctuation at the end. |

### 9.2 Icon buttons

| ID | Rule |
|---|---|
| CMP-IBTN-1 | An icon button is a 40 px circle (small: 32 px, with a 20 px icon). It has no fill at rest. Hover: `--additive`. Pressed: `--chip-hover`. Disabled: the icon is `--text-dis`, with no fill. |
| CMP-IBTN-2 | Every icon button MUST have an accessible name and a tooltip. The tooltip names the action and its shortcut. |
| CMP-IBTN-3 | A count badge sits 2 px from the top and 20 px from the left of the button. It is at least 20 px square, with radius 22 px, `--indicator` fill, white 400 12/16 text, and a 2 px border in the color of the surface behind it. The count includes failures that need the user. Hide it when the count is 0. |

### 9.3 Chips

| ID | Rule |
|---|---|
| CMP-CHIP-1 | A chip is 32 px high, with radius 8 px, padding 0 12 px and 500 14/20 text. Inactive: `--additive`. Hover: `--chip-hover`. Active: `--inverted` fill with `--text-inv` text. |
| CMP-CHIP-2 | In a filter bar, only one chip is selected at a time. The first chip is "All". Selecting a chip highlights its matches on the stage and moves the camera to fit them. "All" clears the highlight and fits the whole stage. |
| CMP-CHIP-3 | When the chips do not fit, the chip bar scrolls sideways. Show a previous icon button at the start of the bar and a next icon button at the end. Each one sits in a 72 px area: the outer half is solid `--base`, then a 36 px fade to transparent. Hide an arrow when there is nothing more to scroll on its side. |
| CMP-CHIP-4 | Answer choices in a card are chips. The selected answer shows the active state. |
| CMP-CHIP-5 | An outlined info chip shows a model, a mode or a context. It is 28 px high, with radius 14 px, a 1 px `--outline` border, 500 12/18 `--text-2` text and a 16 px icon. It is not interactive, except that it shows a tooltip. |

### 9.4 Search field

| ID | Rule |
|---|---|
| CMP-SRCH-1 | The search field has two joined parts, both 40 px high. The input has `--search-bg` fill, a 1 px `--search-border` border, radius 40 px 0 0 40 px (round at the left), padding 0 4 px 0 16 px, 400 16/24 text and a `--text-2` placeholder. The button is 64 x 40 px, with radius 0 40 px 40 px 0 (round at the right), `--search-btn` fill and a 1 px `--search-btn-border` border. |
| CMP-SRCH-2 | At rest, the input starts 32 px in from the left. On focus, it grows to the left over 350 ms and shows a 20 px search icon at its start. It also gets a `--search-focus` border and a shadow along its inner edge (`inset 0 1px 2px rgba(0,0,0,.3)`). |
| CMP-SRCH-3 | Suggestions open 4 px below the field. The list has `--raised` fill, a 1 px `--outline` border, radius 12 px, padding 16 px 0 8 px, and a maximum height of min(460 px, 70vh). Group labels are 500 12/18 `--text-2`. Each option is at least 32 px high, with 400 16/24 text. The part that the user typed stays weight 400. The rest of the suggestion is bold 700. A hint sits at the right, in 12 px `--text-2`. A 12/18 footer sits at the bottom, below a 1 px divider. |
| CMP-SRCH-4 | Keys: `/` moves focus to the field from anywhere. Down and Up move through the options. Enter applies the option or runs the search. Each press of Escape does the next step: close the list, then clear the text, then leave the field. |
| CMP-SRCH-5 | A clear button (a 32 px icon button) shows only when the field has text. |
| CMP-SRCH-6 | Search hits in text use `<mark>`, with `--primary` at 28% over the surface and radius 2 px. |

### 9.5 Masthead status controls

| ID | Rule |
|---|---|
| CMP-STAT-1 | A segmented pill joins a state toggle and a value in one control. It is 36 px high, with radius 18 px and `--additive` fill. Each segment is 500 14/36, with `--chip-hover` on hover. A 1 x 24 px `--outline` divider separates the segments. |
| CMP-STAT-2 | While work runs, the state toggle shows a 20 px pause icon and the word "running". While work is paused, it shows a play icon and the word "paused". The label names the current state, not the action. The tooltip names the action. |
| CMP-STAT-3 | A mode badge is 32 px high, with radius 8 px, `--additive` fill, 500 12/18 text and an 18 px icon. In a degraded mode, the badge changes to `--danger-bg` fill with `--danger` text and icon, and its tooltip says what is degraded. |

### 9.6 Badges and dots

| ID | Rule |
|---|---|
| CMP-BDG-1 | A status badge is 22 px high, with radius 4 px, padding 0 6 px and 500 12/18 text. Neutral: `--additive` fill and `--text-2` text, for normal and waiting states. Danger: `--danger-bg` fill and `--danger` text, for failed, broken or degraded states. OK: `--green` text on the neutral fill. Live: `--indicator` fill and white text. |
| CMP-BDG-2 | A status dot is 8 px. Yellow (`--needs-input`) means needs input, including a failure that needs the user. Cyan (`--primary`) means new results since you last looked. Gray (`--text-2`) and blinking means working. No dot means idle. A status dot is never orange. Only a dot that shows the health of one thing itself, such as a service (CMP-SET-7) or an actor (CMP-AV-4), may be orange. |
| CMP-BDG-3 | A row with several states shows one dot, for the most urgent state: yellow first, then cyan, then gray. A failure counts as needs input. Use this order everywhere that one indicator stands for several states. |
| CMP-BDG-4 | An aggregate indicator stands for many items at once (for example, the tint of a collapsed menu button). It uses the order of CMP-BDG-3. It is yellow if any item needs input or has failed. If not, it is cyan if any item has new results. If not, it is gray and blinking if any item is working. If not, it has no tint. Its tooltip and accessible name name the kinds and counts in words, for example "Flume: 1 failed run, 2 questions". While the list that it stands for is open, the indicator is neutral. |
| CMP-BDG-5 | A duration or zoom badge on media has `rgba(0,0,0,.6)` fill, radius 4 px, padding 1 px 4 px and white 500 12/18 text. |

### 9.7 Avatars

| ID | Rule |
|---|---|
| CMP-AV-1 | Avatars are circles of 24, 36, 40 or 48 px. The letter is weight 500, at about 40% of the diameter. |
| CMP-AV-2 | Actors (agents, services, bots) use a neutral avatar: `--outline-opaque` fill with a `--text` letter. The letter names the kind of actor. Color never does (COL-8). |
| CMP-AV-3 | The user's own avatar has `--inverted` fill with a `--text-inv` person icon or initial. In a feed, the user's name is a chip with `--inverted` fill, `--text-inv` text, radius 2 px and padding 1 px 4 px. The inverted fill is a contrast treatment, not a color with a meaning, so yellow keeps its one meaning (COL-3). |
| CMP-AV-4 | A state dot sits at the bottom right of an actor avatar. It is 34% of the diameter (at least 8 px), with a 2 px ring in the color of the surface behind it. Orange means the actor stopped with an error. Gray and blinking means working. Yellow means needs input. No dot means idle. |
| CMP-AV-5 | The avatar of a tool or system row uses an `--additive` fill, with its content in `--text-2`. |
| CMP-AV-6 | A merge-conflict resolver is an actor like any other (CMP-AV-2), but is marked so it is never mistaken for one: its state dot (CMP-AV-4) is a rounded square, not a circle, and its avatar's letter is always "R". Its header and row (CMP-NAV-5, panel title) carry the text label "Resolver" next to the name. No new color: the dot's color still follows CMP-AV-4's states. |

### 9.8 Navigation rail

| ID | Rule |
|---|---|
| CMP-NAV-1 | Collapsed, the rail is 72 px wide, with padding 4 px. Each entry is 64 px wide, with padding 16 px 0 14 px and radius 10 px. The icon sits above a 400 10/14 label, with a 6 px gap. |
| CMP-NAV-2 | Expanded, the rail is 240 px wide, with padding 12 px. Each entry is 40 px high, with padding 0 12 px, radius 10 px, a 24 px icon, a 24 px gap and 400 14/20 text. |
| CMP-NAV-3 | Collapsed, the current entry shows only its filled icon, with no fill. Expanded, the current entry has the `--additive` fill, weight 500 and the filled icon. Hover: `--additive`. Pressed: `--outline`. |
| CMP-NAV-4 | A 1 px `--outline` line with 12 px padding separates the sections. Section titles are 500 16/22. A footer is 400 12/18 `--text-2`. |
| CMP-NAV-5 | A list of items the user can switch between (projects, workspaces) goes at the top of the expanded rail. Each row is at least 48 px high, with radius 10 px. It has a 28 px tile (radius 6 px, `--outline-opaque`, initial letter 500 13 px), the name in 500 14/20, a status line in 400 12/16 `--text-2`, and a status dot at the end (CMP-BDG-2). Switching changes only the view. |
| CMP-NAV-6 | The menu button opens and closes the expanded rail. While the rail is collapsed, the menu button carries the aggregate indicator (CMP-BDG-4). |
| CMP-NAV-7 | The navigation rail is the only way into settings. No other control opens settings: not a menu header, not a status control, not a badge. A control that shows a setting's value, such as the concurrency cap, only shows it. |

### 9.9 Panels

| ID | Rule |
|---|---|
| CMP-PNL-1 | A panel has a 1 px `--outline` border, radius 12 px and `--base` fill. Its content is cut off at the rounded corners. |
| CMP-PNL-2 | The panel header is 48 px high, with padding 0 4 px 0 16 px and a 1 px divider at the bottom. Its title is 400 16/22. An overflow icon button (for more actions) or a state label sits at the end. |
| CMP-PNL-3 | A feed switch under the header is a chip group where only one chip is selected at a time, like radio buttons. Its padding is 12 px 16 px 8 px. |
| CMP-PNL-4 | The panel body scrolls, with a thin scrollbar. New content in the body enters over 200 ms. |
| CMP-PNL-5 | A detail view starts with a 700 18/26 title, a 12/18 subtitle and a row of status badges 6 px apart. Sections follow with 20 px spacing, with 500 16/22 headings. Key-value pairs are a two-column grid, with gap 4 px 16 px and 13/20 text. Keys are `--text-2`. |

### 9.10 Rows, feeds and messages

| ID | Rule |
|---|---|
| CMP-ROW-1 | A list row has padding 6 px 16 px and a 12 px gap. The title is 500 14/20 and the subtitle is 400 12/18 `--text-2`. Both stay on one line and end with an ellipsis when too long. Hover: `--additive-soft`. Pressed: `--additive`. A row can have a 64 x 36 px thumbnail with radius 4 px. |
| CMP-ROW-2 | A message has padding 4 px 16 px, 13/20 text and a 24 px avatar with a 12 px gap. The timestamp (11 px `--text-3`), the author (weight 500) and the text run together as one flowing line. On a `--suggested` row, the timestamp uses `--text-2`. Tool output uses mono 12/20 `--text-2`. |
| CMP-ROW-3 | A live feed lists the newest event first. Each row has padding 5 px 16 px, a 10 px gap and 13/20 text. It has a 52 px timestamp column (11 px `--text-3`, or `--text-2` on a `--suggested` row), a 24 px avatar and the text. The row also gives the actor name to screen readers. |
| CMP-ROW-4 | Each feed row is a button. It takes the stage to the time and the object of the event. |
| CMP-ROW-5 | When the view shows the past, the feed header reads "Viewing HH:MM:SS" with a gray dot. The current row has the `--suggested` fill, and later rows show at 38% opacity. At the live edge, the header reads "Live" with a violet dot. |
| CMP-ROW-6 | A media lockup has a 168 x 94 px thumbnail with radius 8 px, a duration badge at the bottom right, and an optional 3 px progress strip along the bottom (track: white at 30%). The title is 500 14/20, at most 2 lines. The metadata is 12/18 `--text-2`. An expandable lockup shows a "more" chevron and opens its detail in place. |

### 9.11 Pending action cards

A pending action card holds a request that needs the user: an approval or a question.

| ID | Rule |
|---|---|
| CMP-CARD-1 | A card has `--additive` fill, radius 12 px and padding 10 px 14 px 12 px. Up to two cards sit side by side in a two-column grid, with a 12 px gap. |
| CMP-CARD-2 | The header row has a 24 px avatar, `@name` in 500 13/18, the time in 12/18 `--text-2`, and a pin label at the end (14 px pin icon, 12/18 `--text-2`). The card text is 14/20. Metadata is 12/18 `--text-2`. The actions row sits 8 px below, with size S buttons 8 px apart. |
| CMP-CARD-3 | An approval offers a filled recommended choice and tonal alternatives. Choosing one does not act at once. Instead, the actions row changes to a confirm step. The step shows a 16 px warning icon in `--danger`, a one-line consequence in `--danger` 12/18, a primary Confirm button and a text Cancel button. Focus moves to Confirm. Cancel brings back the choices and puts focus back where it was. |
| CMP-CARD-4 | Cards keep a stable order. Resolving one card MUST NOT move another card under the pointer. Put the card that stays longer first. |
| CMP-CARD-5 | Cards enter and leave with the card motion (section 8.2). When the user opens a card from somewhere else (a badge, a notification), scroll the card into view, focus its first enabled button and flash it. The flash is a 2 px `--primary` ring and the `--suggested` fill for 1.4 s. |
| CMP-CARD-6 | When the view shows the past, the card's actions are disabled. A 24 px high text-primary button, "Go live to answer", takes the place of the pin label. |
| CMP-CARD-7 | After the user resolves an approval, show a snackbar that states the result and offers Undo. An answered question does not use a snackbar. It shows the answer and the reply in the conversation instead. |

### 9.12 Text input (composer)

| ID | Rule |
|---|---|
| CMP-FLD-1 | An underline field has no box. A 1 px `--outline` line runs along its bottom. On focus, a 2 px `--text` line grows out from the center to full width (250 ms `--ease-std`). On error, the 2 px line is `--danger`. |
| CMP-FLD-2 | A text area grows from 24 px to 120 px high, then scrolls. Its text is 400 14/20. The placeholder is `--text-2`, and it names the keys to use. |
| CMP-FLD-3 | Enter sends. Shift+Enter adds a line. Do not send while an input method editor (IME) is still composing text. |
| CMP-FLD-4 | Helper and error text is 400 12/18, with a 16 px icon and a 4 px gap. Error text is `--danger`. This text is an assertive live region, so screen readers announce it at once. Sending an empty message shows an error, instead of silently doing nothing. |
| CMP-FLD-5 | The composer row has a 40 px user avatar, a 16 px gap, the field, then Cancel and Send, 8 px apart. Cancel is a text button that shows only while there is something to cancel. Send is a primary button. A 12/18 `--text-2` line under the field MAY show usage or attribution. |

### 9.13 Menus and notifications

| ID | Rule |
|---|---|
| CMP-MENU-1 | A menu has `--menu` fill, radius 12 px, `--shadow-menu`, padding 8 px 0 and a minimum width of 220 px. It opens directly below its trigger, with no gap, and slides down 4 px into place. |
| CMP-MENU-2 | Each item is 36 px high, with padding 0 36 px 0 16 px, a 16 px gap and 400 14/20 text. A 24 px check column is always there. An unchecked item hides the check but keeps its space. Hover and focus: `--additive`. |
| CMP-MENU-3 | When the menu opens, focus the first item. Down and Up move between items and wrap around at the ends. Escape closes the menu and returns focus to the trigger. The trigger has `aria-expanded`. |
| CMP-MENU-4 | The notification menu is 480 px wide (at most 100vw - 32 px). Its maximum height is min(560 px, 100vh - 80 px). Its header is 48 px high, with a 400 16/22 title and no buttons. It has no way into settings (CMP-NAV-7). Each item has padding 16 px 16 px 16 px 0, a 4 px `--primary` dot when unread, a 48 px avatar, 14/20 text, the time and context in 12/18 `--text-2`, and an 86 x 48 px thumbnail (radius 8 px) at the end. |
| CMP-MENU-5 | Choosing a notification closes the menu and takes the user to the object (CMP-CARD-5). The trigger icon is filled while the menu is open. |
| CMP-MENU-6 | The notification menu lists each item on its own. On the stage, items for the children of a collapsed parent add up to one count on the parent (STG-DRAW-8). The menu does not combine them. |

### 9.14 Tooltips

| ID | Rule |
|---|---|
| CMP-TIP-1 | A page tooltip has `--tooltip` fill, white text, radius 4 px, padding 8 px, 400 12/18 text and a maximum width of 300 px. |
| CMP-TIP-2 | A stage tooltip has `--stage-popup` fill, radius 8 px and 500 13 px text. |
| CMP-TIP-3 | A rich tooltip (for stage objects) has `--stage-popup` fill, radius 8 px, padding 10 px 12 px and a maximum width of 280 px. The title is 500 14/20. Below it is a two-column list of labels and values, with gap 0 12 px and labels in white at 70%. A last line, in white at 70%, says what a click does. |
| CMP-TIP-4 | A tooltip shows after 400 ms of hover. It shows at once on keyboard focus (`:focus-visible`). Rich tooltips on stage objects show at once and follow the pointer. A tooltip hides when the pointer is pressed, when the pointer leaves, and when focus leaves. It enters over 150 ms, growing from scale 0.96. It leaves over 100 ms. |
| CMP-TIP-5 | By default, place the tooltip below its control. For controls at the bottom of the stage, place it above. For controls at the right edge of the stage, place it to the left. Rich tooltips follow the pointer at an offset of +16 px across and +18 px down. Keep tooltips 8 px from the window edge. |
| CMP-TIP-6 | The text names the action, with the shortcut in parentheses: "Zoom in (+)". |

### 9.15 Snackbar

A snackbar is a short message bar that confirms a result.

| ID | Rule |
|---|---|
| CMP-SNK-1 | The snackbar sits at the bottom left, with a 16 px margin. It has `--inverted` fill, `--text-inv` text, radius 8 px, a minimum height of 48 px, a minimum width of 288 px and a maximum width of min(560 px, 100vw - 32 px). Its text is 400 14/20, with margin 8 px 12 px 8 px 16 px. |
| CMP-SNK-2 | A snackbar has at most one action: a text button in `--primary-inv`. The usual action is Undo. |
| CMP-SNK-3 | Show one snackbar at a time. A new one replaces the old one at once. |
| CMP-SNK-4 | A snackbar stays for 5 s. It pauses while the pointer is over it, and closes 2 s after the pointer leaves. It is a polite live region: screen readers announce it without interrupting the user. |

### 9.16 Dialog

| ID | Rule |
|---|---|
| CMP-DLG-1 | A modal dialog has `--scrim` behind it (black at 0.5 in the dark theme, 0.4 in the light theme). It has `--menu` fill, radius 12 px and `--shadow-menu`. Its size is min(760 px, 100vw - 48 px) wide by min(600 px, 100vh - 48 px) high. |
| CMP-DLG-2 | The header has padding 20 px 16 px 12 px 24 px. It holds the title (700 20/28), a description (400 14/20 `--text-2`) and a close icon button at the end. |
| CMP-DLG-3 | A dialog with sections has a 200 px vertical list of tabs and a scrolling content area. Each tab is 40 px high, with radius 10 px, a 20 px icon and a 16 px gap. The selected tab has `--additive` fill and weight 500. The content area has padding 8 px 24 px 24 px. 1 px `--outline` lines separate the header, the tab list and the content area. |
| CMP-DLG-4 | The footer is right-aligned, with padding 12 px 16 px and a divider at its top. It has one text-primary "Done" button. Settings take effect at once, so there is no Save button. |
| CMP-DLG-5 | Escape or a click on the scrim closes the dialog. When it opens, focus the selected tab. When it closes, return focus to the control that opened it. |
| CMP-DLG-6 | A settings dialog holds configuration only. It MUST NOT show work state or repeat what the stage shows. |

### 9.17 Settings controls

| ID | Rule |
|---|---|
| CMP-SET-1 | A setting row has padding 16 px 0 and a 1 px `--outline` line at its bottom (not on the last row). The label is 500 14/20. The current value sits at the right, in 500 14/20 with tabular numerals. The description is 400 13/18 `--text-2`, 4 px below. |
| CMP-SET-2 | A slider has a 24 px hit area (the area that responds to the pointer) and a 4 px track (radius 2 px). The track is `--primary` up to the value and `--outline` after it. The thumb is a 14 px `--primary` circle. On hover, it has an 8 px halo of primary at 18%. When pressed, the halo is 10 px at 26%. On focus, it has a 3 px ring in the surface color plus a 2 px `--text` ring. Tick labels sit below, in 12/18 `--text-2` with tabular numerals. |
| CMP-SET-3 | A recommended value shows as a 2 x 18 px mark on the track (`--text` at 70%), with a matching legend "Recommended: value". When the value moves away from the recommendation, a size S text-primary button, "Return to recommendation", appears with an undo icon. If the recommendation was not measured, the legend says so. |
| CMP-SET-4 | A switch is 36 x 20 px. Its track is 14 px high, with radius 7 px: `--text-dis` at 60% when off, `--primary` at 50% when on. Its thumb is 20 px: `#EDEFF7` when off, `--primary` when on. The thumb moves 16 px in 150 ms `--ease-std`. On hover, it has an 8 px `--additive` halo. Use role `switch` with `aria-checked`. |
| CMP-SET-5 | A single-choice list has 40 px rows, with radius 10 px and padding 0 12 px 0 8 px. Each row has a 24 px check column, then a 20 px leading icon in `--text-2`. Use role `radiogroup`. Up and Down move between the rows. |
| CMP-SET-6 | A note has `--additive` fill, radius 12 px, padding 12 px 16 px and 400 13/18 `--text-2` text. Use it to state a fixed behavior, not a warning. |
| CMP-SET-7 | A status list (providers, services) has rows with padding 10 px 0 and dividers. Each row has an 8 px status dot, the name in 500 14/20, the state in 500 12/18, and a detail in 12/18 `--text-2` on a second line. The dot and the state text are `--green` when healthy and `--danger` when degraded or failed. |

### 9.18 Empty states

| ID | Rule |
|---|---|
| CMP-EMP-1 | An empty state is centered. It has a 72 px `--additive` ring with a 36 px `--text-2` icon, or a bare 64 px icon. The title is 500 16/22. The text is 400 14/20 `--text-2`, at most 300 px wide. |
| CMP-EMP-2 | The title says the state in plain words ("Nothing needs you right now"). The text says what will show up here and where it comes from. |
| CMP-EMP-3 | A no-results state repeats the query in the title and suggests one next step. |

## 10. The stage

### 10.1 Frame and overlays

| ID | Rule |
|---|---|
| STG-1 | The stage is `--stage-bg` in every theme, with radius 12 px. Its content is cut off at its rounded edge. All overlay text and icons on the stage are white. |
| STG-2 | The top gradient is 84 px high and goes from `rgba(0,0,0,.55)` to transparent. It shows only when something is selected. |
| STG-3 | The selection title sits at the top left, 12 px in from the edges. It has a 40 px avatar, the title (500 18/24), metadata (400 13/18, white at 70%) that says how to clear the selection (for example "Esc clears"), and then a clear button in the popup style. Its maximum width is the stage width minus 300 px. It enters in 250 ms, sliding down from 6 px above. |
| STG-4 | The bottom gradient is 110 px high and goes from transparent to `rgba(0,0,0,.72)`. It sits behind the control bar. |
| STG-5 | The control bar is 48 px high and sits 12 px in from each side, with the timeline scrubber above it. The bar holds one group of view controls at the right, 2 px apart: overlay toggles, zoom out, zoom in, fit. There are no playback controls: no replay, speed, live badge, time readout or play button. |
| STG-6 | Stage buttons are 40 px circles with a white icon and no fill. Hover: white at 10%. Pressed: white at 20%. Disabled: opacity 0.3. Focus: a 2 px white ring just inside the edge. A popup-style button has a `--stage-popup` fill. |
| STG-7 | The legend sits at the top right, 12 px in from the edges. It is 220 px wide, with `--stage-popup` fill, radius 12 px and 400 12/16 text. Its header is 32 px high, in 500 13/18, with a chevron that collapses it. The legend collapses on its own when it would overlap the minimap. If the user opens or closes it, that choice wins. |
| STG-8 | The minimap is a small overview of the whole stage. It sits at the bottom right and is 172 px wide, with radius 8 px, a 1 px border in white at 30%, and a `--stage-bg` background. It shows all nodes as 1 px dots in white at 50%, actors as 4 px dots in `#EDEFF7`, and the current view as a white 1.5 px frame with a fill of white at 8%. A click or a drag moves the view. When the minimap has focus, the arrow keys move the view. A zoom badge (CMP-BDG-5) sits in its corner. The minimap moves down when the chrome hides. |
| STG-9 | Retired. It was the play and pause bezel (a large play or pause icon on the stage). There is no playback. |

### 10.2 Chrome auto-hide

| ID | Rule |
|---|---|
| STG-HIDE-1 | Moving the pointer, or moving focus, inside the stage shows the chrome. |
| STG-HIDE-2 | The chrome hides 2.6 s after the last activity, but only when all of these are true: the view is at the live edge, no drag is in progress, focus is not on a stage control, and the pointer is not over the control bar. If the pointer is over the control bar, check again after 1.2 s. |
| STG-HIDE-3 | When the pointer leaves the stage, hide the chrome after 0.4 s. |
| STG-HIDE-4 | To hide the chrome, fade out the controls and the bottom gradient over 100 ms. Move the timeline down 40 px and shrink its knob to scale 0. Move the minimap down to 14 px from the bottom. |
| STG-HIDE-5 | When the view is not at the live edge, the chrome stays visible. |

### 10.3 Timeline scrubber

| ID | Rule |
|---|---|
| STG-TL-1 | The scrubber's hit area is 15 px high. The bar is 3 px high, and 5 px on hover, drag or focus. |
| STG-TL-2 | The bar is split into named segments (chapters), with a small gap between them. The two outer ends have 2 px rounded corners. The part after the viewed time is white at 20%. On hover, the part up to the pointer is white at 50%. The part up to the viewed time is `--progress`, drawn as one gradient across all segments. |
| STG-TL-3 | The knob is a 13 px `--accent` circle. It is at scale 0 (not visible) at rest, and at scale 1 on hover, drag or focus. It is always at scale 1 when the view is behind live. |
| STG-TL-4 | On hover, a preview shows above the bar: a 160 x 90 px thumbnail with radius 8 px and a 2 px white border, then the segment name and the time in 500 13/18 white with a text shadow. |
| STG-TL-5 | Retired and replaced by STG-TL-7. It was the live badge. |
| STG-TL-6 | Keys on the timeline: Left and Right step one event. Home goes to the start. End goes to live. The timeline has role `slider`, with value attributes. |
| STG-TL-7 | To return to live, click or drag to the right end of the scrubber, or press End. While the view is behind live, the knob stays visible and the feed header says "Viewing HH:MM:SS" (CMP-ROW-5). |

### 10.4 Drawing rules

| ID | Rule |
|---|---|
| STG-DRAW-1 | Each population (each kind of object) has one shape. For example: dots for items, rounded cards for tasks, circles with a letter for actors. |
| STG-DRAW-2 | Item dots are gray. Their radius grows with the square root of their number of connections. A search or filter match is cyan. |
| STG-DRAW-3 | When a highlight is active, everything outside it is drawn at 15% of its normal opacity. Relation arrows and count pills outside it are drawn at 40% of normal, and parent-child links at 35% of normal. |
| STG-DRAW-4 | Hover: a 1 px white ring at 55% to 60% opacity, 3 to 5 px outside the shape. Selection: a 2 px cyan ring, and the selection's direct relations draw in cyan at 55%. A match: a 1.5 px cyan ring. |
| STG-DRAW-5 | A task card is `#171A26`, with a 1 px border in white at 34% and radius 6 px (4 px when small). A progress strip runs across its bottom, 4 px high (3 px when small), with a track of white at 22% and a `--progress` fill. A done task shows a white check mark. A collapsed parent shows "+N" at its top left, in 500 10 px, white at 70%. |
| STG-DRAW-6 | A waiting item draws at 50% opacity. The reason is a sub-label in words under its name ("blocked by X", "queued"). A thin gray arrow points from the waiting item to its cause. The arrow is at 30% opacity, or 95% when either end has focus. |
| STG-DRAW-7 | An actor is a disk with a 12 px radius, `#1E2233` fill, a 1.2 px border in white at 60% and a 500 12 px letter. A state dot sits at its lower right, with a black ring (CMP-AV-4). A relation that the actor holds is a dashed gray line (dash pattern 2 on, 4 off). The line is solid cyan when the actor is selected. |
| STG-DRAW-8 | A needs-input item shows a yellow count pill at the top right of its card. The pill is 18 px high, with radius 9 px, a 3 px black outline and dark 500 11 px text. When the item is hidden inside a collapsed parent, its count adds to the pill on the nearest visible ancestor. A failed item hidden inside a collapsed parent also adds to that yellow pill, because a failure needs the user. The pill can be clicked, and it opens the matching cards (CMP-CARD-5). |
| STG-DRAW-9 | Labels are 12 px, with the text halo. Place labels in order of priority, first come first placed. A task label tries these places in order: below, right, left, then above its card. An actor label tries right, left, above, then below. A label that fits nowhere is not drawn. Hovered and selected labels always draw. A sub-label is 11 px, white at 62%, under its label. It is centered under a task label, and aligned to the side of an actor label. |
| STG-DRAW-10 | Group labels sit under their group, in 400 12 px. Below 2 x the fit scale, show the short name. Above it, show the full name (STG-CAM-1). |
| STG-DRAW-11 | A failed or broken item is orange on the stage, on the item itself. A failed task card keeps its shape and its progress strip stops where it failed. The card gets a 1.5 px `--stage-danger` ring and an 18 px `--stage-danger` pill at its top left, with a dark `#0D0F17` 12 px error icon, a 3 px black outline and the same click behavior as the count pill (STG-DRAW-8). The reason is a sub-label in words, for example "failed: 3 tests". A failed actor gets the orange state dot (CMP-AV-4). A broken item dot is drawn in `--stage-danger`. When the failed item is hidden inside a collapsed parent, it counts in the yellow pill on the parent instead (STG-DRAW-8). |

### 10.5 Camera

| ID | Rule |
|---|---|
| STG-CAM-1 | Camera scale is the number of screen pixels per world unit (the stage's own unit of distance). The fit scale is the camera scale after "Fit": it frames all content and keeps it clear of the overlays (legend, control bar). The zoom range goes from 0.6 x the fit scale to a camera scale of 5. |
| STG-CAM-2 | Wheel zoom is continuous, and it keeps the point under the pointer in place. Buttons and keys zoom by 1.4 x per step, with animation. Pinch (ctrl + wheel) zooms faster. |
| STG-CAM-3 | A drag starts after the pointer moves 3 px while pressed. A press without a drag is a click. A click on empty space clears the selection. |
| STG-CAM-4 | When the user selects an object from outside the stage (a feed row, a search result), the camera moves to it in 500 ms. |
| STG-CAM-5 | When the stage changes size, fit again only if the view was at fit. If not, keep the user's view. |
| STG-CAM-6 | Keys on the stage: the arrow keys pan 80 px, `+` and `-` zoom, `0` fits, and Escape clears. |

### 10.6 Performance

| ID | Rule |
|---|---|
| STG-PERF-1 | Redraw only when something changes, not every frame. Keep the frame loop running only while something animates. |
| STG-PERF-2 | A blinking state needs about 15 redraws per second, not 60. |
| STG-PERF-3 | Limit the device pixel ratio to 2. Do not draw shapes that are outside the view (with a 40 px margin). |
| STG-PERF-4 | A software-rendering mode says so in a mode badge (CMP-STAT-3). If the mode leaves out a costly effect, the badge tooltip names the effect. |

## 11. Interaction

| ID | Rule |
|---|---|
| INT-1 | The focus ring is a 2 px solid `--text` line, with offset 2 px (outside the element). Inside filled or clipped containers, the offset is -2 px (inside the edge). On the stage, the ring is white. On an active chip, the ring is `--base` and sits inside the edge (offset -4 px). Show it only for `:focus-visible`. |
| INT-2 | Hover and pressed states change the fill. They do not add borders or shadows. Hover is `--additive`, or `--chip-hover` on an element that already has an additive fill. Pressed is one step stronger: `--chip-hover`, or `--outline` on a tonal fill. |
| INT-3 | Escape works in steps. Each press closes one layer, the top one first: the open menu, then the tooltip, then the selection, then the active filter, then the active search. |
| INT-4 | Single-key shortcuts MUST NOT fire while the user types in a field or holds Ctrl, Cmd or Alt. |
| INT-5 | The standard single-key shortcuts are `/` for search, `m` for the minimap, and `+` `-` `0` for zoom and fit. Do not add playback keys. |
| INT-6 | A click that opens a related object moves focus to it and scrolls it into view (smoothly, or at once when reduced motion is on). |
| INT-7 | The minimum target size is 24 x 24 px (WCAG 2.2 AA). Buttons, chips and icon buttons are 32 px high or more. A thin target that spans its full row, such as the scrubber or a dense list row, MAY be shorter. |
| INT-8 | An action with significant consequences has a two-step confirm in place (CMP-CARD-3). A completed action has Undo (CMP-SNK-2). Do not use a modal dialog to confirm. |

## 12. Content and voice

| ID | Rule |
|---|---|
| TXT-1 | Write short, direct sentences in sentence case. Do not use exclamation marks. |
| TXT-2 | State a status in words, with its reason: "Held by cap", "Blocked by w2", "Idle". |
| TXT-3 | Show times in 24-hour `HH:MM:SS`. Write relative dates as "today at HH:MM:SS". |
| TXT-4 | A count agrees with its noun: "1 agent", "3 agents", "2 pending". |
| TXT-5 | A tooltip says what a control does, not what it is: "Pause the scheduler". Then add one sentence about the effect if the effect is not obvious. |
| TXT-6 | A disabled or degraded state says why, and what to do about it. |
| TXT-7 | Do not claim a benefit that the product cannot measure. |

## 13. Theming

| ID | Rule |
|---|---|
| THM-1 | Dark is the default. Offer three choices: device theme, dark and light. |
| THM-2 | Switch themes with one attribute on the root element (`data-theme="dark"` or `"light"`). Set `color-scheme` to match. |
| THM-3 | The stage, its overlays and its tooltips do not change with the theme. |
| THM-4 | Components take theme colors only from tokens. A fixed color is allowed only where this guide states it: sections 3.3 and 3.4, and the literal color values in component and stage rules. Any other color literal in the code is a defect. |

## 14. Accessibility

| ID | Rule |
|---|---|
| A11Y-1 | All text meets WCAG 2.2 AA contrast on its surface in both themes: 4.5:1, or 3:1 for large text. Non-text parts that carry meaning meet 3:1. Disabled text and decorative borders are exempt. Section 14.1 lists the measured pairs. Measure any new pair of tokens before you use it. |
| A11Y-2 | Every control has an accessible name. Controls that show only an icon use `aria-label`. |
| A11Y-3 | Use native roles, or the matching ARIA pattern: `combobox` and `listbox` for search, `menu` and `menuitemcheckbox` for menus, `tablist` for dialog sections, `radiogroup` for feed switches and single-choice lists, `switch` for toggles, `slider` for the timeline, and `dialog` with `aria-modal` for modals. |
| A11Y-4 | Live feeds are polite live regions (announced without interrupting). Form errors are assertive (announced at once). The snackbar host is a polite status region. |
| A11Y-5 | The stage canvas can take focus. Its accessible name lists its keys. The legend is its description. |
| A11Y-6 | A state shown only by color (dots, rings) also appears in the accessible name or the tooltip. |

### 14.1 Measured contrast

These are the WCAG 2.2 contrast ratios for the token pairs that this guide prescribes. A
translucent color is blended over the surface it sits on before it is measured. Stage pairs are
the same in both themes. The Needs column is the lowest ratio allowed. Measured with
`contrast.py`.

| Pair | Dark | Light | Needs |
|---|---|---|---|
| Body text on page | 16.66 | 17.48 | 4.5:1 |
| Body text on menu | 13.94 | 18.55 | 4.5:1 |
| Body text on tonal fill | 12.98 | 15.79 | 4.5:1 |
| Body text on hover fill | 9.15 | 14.21 | 4.5:1 |
| Body text on highlighted row | 12.13 | 16.48 | 4.5:1 |
| Body text on card over menu | 10.28 | 16.75 | 4.5:1 |
| Secondary text on page | 7.34 | 5.91 | 4.5:1 |
| Secondary text on menu | 6.14 | 6.27 | 4.5:1 |
| Secondary text on raised | 6.65 | 6.27 | 4.5:1 |
| Secondary text on card | 5.72 | 5.34 | 4.5:1 |
| Secondary text on highlighted row | 5.35 | 5.57 | 4.5:1 |
| Feed timestamp (text-3) on page | 6.04 | 4.76 | 4.5:1 |
| Feed timestamp on highlighted row (text-2) | 5.35 | 5.57 | 4.5:1 |
| Filled button / active chip text | 16.66 | 18.55 | 4.5:1 |
| Filled button text, hover | 13.31 | 14.28 | 4.5:1 |
| Primary button text | 10.58 | 5.36 | 4.5:1 |
| Primary button text, hover | 13.20 | 7.27 | 4.5:1 |
| Text-primary button on page | 10.58 | 5.05 | 4.5:1 |
| Text-primary button on menu | 8.85 | 5.36 | 4.5:1 |
| Snackbar text | 16.66 | 18.55 | 4.5:1 |
| Snackbar action (primary-inv) | 4.67 | 10.27 | 4.5:1 |
| Danger text (form error) on page | 8.45 | 5.36 | 4.5:1 |
| Danger badge text | 6.58 | 5.01 | 4.5:1 |
| Confirm prompt (danger) on card | 6.58 | 4.85 | 4.5:1 |
| OK badge text (green) on neutral badge | 7.75 | 4.67 | 4.5:1 |
| Healthy state text (green) on menu | 8.32 | 5.48 | 4.5:1 |
| Degraded state text (danger) on menu | 7.07 | 5.69 | 4.5:1 |
| Search placeholder (text-2) | 7.50 | 6.27 | 4.5:1 |
| Tooltip text | 12.94 | 10.48 | 4.5:1 |
| User name chip and avatar (text-inv on inverted) | 16.66 | 18.55 | 4.5:1 |
| Count badge text | 5.70 | 5.70 | 4.5:1 |
| Live status badge text | 5.70 | 5.70 | 4.5:1 |
| Live dot and scrubber knob (accent) on page | 4.52 | 3.99 | 3.0:1 |
| Disabled text on page (exempt) | 3.95 | 3.00 | exempt |
| Needs-input dot on page | 12.49 | 4.64 | 3.0:1 |
| Danger dot, icon, underline on page | 8.45 | 5.36 | 3.0:1 |
| Selection ring / new dot (primary) on page | 10.58 | 5.05 | 3.0:1 |
| Healthy dot (green) on page | 9.95 | 5.17 | 3.0:1 |
| Working dot (text-2) on page | 7.34 | 5.91 | 3.0:1 |
| Search focus border on search fill | 5.31 | 5.36 | 3.0:1 |
| Focus ring (text) on page | 16.66 | 17.48 | 3.0:1 |
| Slider and switch (primary) on menu | 8.85 | 5.36 | 3.0:1 |
| Panel border (outline) on page (decorative) | 1.82 | 1.28 | exempt |
| Stage: Stage label, white 88% | 15.34 | 15.34 | 4.5:1 |
| Stage: Stage sub-label, white 62% | 7.76 | 7.76 | 4.5:1 |
| Stage: Stage group label, white 55% | 6.26 | 6.26 | 4.5:1 |
| Stage: Stage overlay meta, white 70% | 9.75 | 9.75 | 4.5:1 |
| Stage: Count pill text on yellow | 12.49 | 12.49 | 4.5:1 |
| Stage: Rich tooltip label, white 70% on popup | 9.10 | 9.10 | 4.5:1 |
| Stage: Selection ring (cyan) on the stage | 11.06 | 11.06 | 3.0:1 |
| Stage: Needs-input yellow on the stage | 13.05 | 13.05 | 3.0:1 |
| Stage: Stage danger orange on the stage | 7.69 | 7.69 | 3.0:1 |
| Stage: Error icon, dark on stage danger pill | 7.36 | 7.36 | 3.0:1 |
| Stage: Progress fill (violet) on the stage | 4.72 | 4.72 | 3.0:1 |
| Stage: Scrubber knob and live dot (accent) on the stage | 4.72 | 4.72 | 3.0:1 |
| Stage: Item dot, cool gray 58%, on the stage | 3.38 | 3.38 | 3.0:1 |

## 15. Assets and licenses

| Asset | License | Note |
|---|---|---|
| Instrument Sans (400, 500, 700), latin woff2 | SIL OFL 1.1 | Ship the files with the app. Do not load fonts from the network at run time. |
| Fragment Mono (400), latin woff2 | SIL OFL 1.1 | Ship the files with the app. |
| Material Symbols Outlined, weight 400, SVG | Apache 2.0 | Copy only the icons in use. Keep the license file next to them. |

| ID | Rule |
|---|---|
| AST-1 | The product ships all fonts and icons in its own source tree. The UI makes no network request for assets. |
| AST-2 | Each product has its own original brand mark. It is a single color, `--text`, with at most one `--accent` element. |
| AST-3 | Record the source, version, date and license of each asset in a `SOURCES.md` file next to the asset files. |
| AST-4 | Keep each font's real family name, including in a subset or a converted copy, so anyone who looks can see what it is. The fonts in this guide declare no reserved font name, so the SIL OFL allows this. A font with a reserved font name MUST NOT be subset under its own name. |

## 16. Rule index by severity

A reviewer checks these rules first. A breach of any of them is a blocking defect.

`COL-1`, `COL-3`, `COL-8`, `COL-9`, `LAY-1`, `MOT-2`, `CMP-BTN-1`, `CMP-IBTN-2`, `CMP-CARD-3`,
`CMP-CARD-4`, `CMP-DLG-6`, `STG-1`, `THM-3`, `THM-4`, `INT-4`, `A11Y-2`, `AST-1`.

## 17. Token block

Paste this block at the root of a stylesheet. In a native toolkit, map each token to a named
constant with the same name.

```css
:root {
  /* Type */
  --font: "Instrument Sans", "Helvetica Neue", Arial, sans-serif;
  --mono: "Fragment Mono", ui-monospace, Menlo, monospace;

  /* Layout */
  --masthead-h: 56px;
  --nav-mini: 72px;
  --nav-full: 240px;
  --secondary-w: clamp(360px, 28vw, 402px);

  /* Fixed colors (both themes) */
  --accent: #8B5CF6;
  --indicator: #7C3AED;
  --progress: linear-gradient(90deg, #8B5CF6 70%, #C084FC);
  --stage-bg: #07080F;
  --stage-popup: rgba(22,25,38,.92);
  --stage-danger: #FF7A33;

  /* Motion */
  --ease-in: cubic-bezier(0,0,.2,1);
  --ease-out: cubic-bezier(.4,0,1,1);
  --ease-std: cubic-bezier(.2,0,0,1);
  --ease-search: cubic-bezier(.2,0,.6,1);
  --ease-spring: cubic-bezier(.34,1.56,.64,1);
}

:root[data-theme="dark"] {
  color-scheme: dark;
  --base: #0D0F17; --raised: #171A26; --menu: #1D2130;
  --inverted: #EDEFF7; --inverted-hover: #D3D7E4;
  --additive: rgba(255,255,255,.1); --additive-soft: rgba(255,255,255,.05); --chip-hover: rgba(255,255,255,.2);
  --outline: rgba(255,255,255,.2); --outline-opaque: #2E3345;
  --text: #EDEFF7; --text-2: #9AA0B4; --text-3: rgba(255,255,255,.54); --text-dis: #6B7186; --text-inv: #0D0F17;
  --primary: #22D3EE; --primary-hover: #67E8F9; --primary-inv: #0E7490; --suggested: #10303B;
  --danger: #FB923C; --danger-bg: rgba(251,146,60,.16); --needs-input: #FACC15;
  --green: #34D399;
  --search-bg: #0A0C13; --search-border: #2A2F40; --search-btn: rgba(255,255,255,.06);
  --search-btn-border: #2A2F40; --search-focus: #0891B2;
  --tooltip: rgba(46,51,69,.94); --scrim: rgba(5,6,12,.6); --scrollbar: #4A5068;
  --shadow-menu: 0 4px 32px rgba(0,0,0,.35);
}

:root[data-theme="light"] {
  color-scheme: light;
  --base: #F7F8FC; --raised: #FFFFFF; --menu: #FFFFFF;
  --inverted: #10131C; --inverted-hover: #262A38;
  --additive: rgba(16,19,28,.05); --additive-soft: rgba(16,19,28,.03); --chip-hover: rgba(16,19,28,.1);
  --outline: rgba(16,19,28,.12); --outline-opaque: #E1E4EE;
  --text: #10131C; --text-2: #5A6072; --text-3: rgba(16,19,28,.6); --text-dis: #8A90A2; --text-inv: #FFFFFF;
  --primary: #0E7490; --primary-hover: #155E75; --primary-inv: #22D3EE; --suggested: #DDF6FB;
  --danger: #B83C0B; --danger-bg: #FFEDE3; --needs-input: #A16207;
  --green: #047857;
  --search-bg: #FFFFFF; --search-border: #C9CEDC; --search-btn: #EEF0F6;
  --search-btn-border: #C9CEDC; --search-focus: #0E7490;
  --tooltip: rgba(46,51,69,.94); --scrim: rgba(16,19,28,.4); --scrollbar: #A3A9BA;
  --shadow-menu: 0 4px 32px rgba(16,19,28,.12);
}

@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after { transition-duration: 0s !important; animation-duration: 0s !important; }
}

body {
  font: 400 14px/20px var(--font);
  color: var(--text);
  background: var(--base);
  -webkit-font-smoothing: antialiased;
  scrollbar-color: var(--scrollbar) transparent;
}
:focus-visible { outline: 2px solid var(--text); outline-offset: 2px; }
.num { font-variant-numeric: tabular-nums; }
```

## 18. Adoption checklist

1. Copy this file into the project (for example into `docs/`). Do not
   change rule IDs. Put project-specific rules in a separate file, with a project prefix on their
   IDs.
2. Bundle the fonts and icons (section 15) and write their `SOURCES.md`.
3. Paste the token block (section 17). Remove every color literal from component code.
4. Name the project's stage: what it draws, its populations (STG-DRAW-1) and its states.
5. Map the project's states to the five meanings: violet for progress, orange for broken or failed,
   yellow for needs input, cyan for selection or new, gray for everything else. A state that fits
   none of these is gray, with a word that names it.
6. Build the shell (section 6), then the stage frame and chrome (section 10), then the components
   that the project needs (section 9).
7. Check the blocking rules (section 16) in both themes and with reduced motion on.
8. Record each deviation with the rule ID and the reason.

## 19. Changes

| Version | Change |
|---|---|
| 1.10 | New rule CMP-AV-6: a merge-conflict resolver's state dot is a rounded square, its avatar letter is "R", and its header and row carry the label "Resolver". No color changed. |
| 1.9 | Typefaces: Instrument Sans (400, 500, 700) for all text and Fragment Mono (400) for code. Both are SIL OFL 1.1 with no reserved font name, and both keep their real names (new rule AST-4). Tables changed: 4.1, 15, 17. |
| 1.8 | The Nocturne palette. Violet replaces red for progress and live. Cyan replaces blue for selection and primary actions. Surfaces are blue-black in the dark theme and cool off-white in the light theme. The stage is `#07080F`. Tokens renamed: `--accent-red` is now `--accent`, `--indicator-red` is now `--indicator`. All measured contrast pairs pass (section 14.1). |
| 1.7 | A failure counts as yellow in every indicator that sums up several items (status dots, the menu button tint, count pills on collapsed parents, the notification count). Orange stays on the failed thing itself. |
| 1.6 | The user's avatar and name chip use `--inverted` fill with `--text-inv`, so yellow keeps its one meaning (COL-3). |
| 1.5 | New rule CMP-NAV-7: the navigation rail is the only way into settings. The notification menu header has no settings button (CMP-MENU-4). |
| 1.4 | Orange is the fifth color: broken, failed or dangerous. New tokens: `--danger`, `--danger-bg`, `--needs-input` and `--stage-danger`. New rule STG-DRAW-11. Waiting states (queued, blocked, held by cap, colliding) are gray. |
| 1.3 | Wording in plain English. No value or requirement changed. |
| 1.2 | Contrast measured for every prescribed token pair (section 14.1). Four contrast failures fixed: the search placeholder, degraded state text, the live status badge and timestamps on highlighted rows. |
| 1.1 | The stage keeps only the timeline scrubber. Playback controls removed. STG-9 and STG-TL-5 retired. STG-TL-7 added. |
| 1.0 | First version. |
