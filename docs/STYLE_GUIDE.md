# kGraf UI Style Guide

This is the UI guide for kGraf's desktop shell. It is for anyone who designs or builds a screen in the shell, or reviews one. The style is called **Stage**. It suits an interface built around one large canvas, the **stage**, which in kGraf is the galaxy graph. The interface keeps attention on the stage, and it offers information density that scales with what the stage shows.

The shell is a native Rust app, not a web page. Section 8 says how the web terms in this guide map to it.

## 1. Principles

These seven principles decide any case that the rules do not cover.

1. **The stage is the product.** The main content sits on a black stage. The page frames the stage and never competes with it. The stage stays black in every theme.
2. **Chrome stays in the background.** Controls on the stage show when the user is active and hide when the user is idle. The content is always the largest and highest-contrast thing in view.
3. **One meaning per color.** Violet is progress and live. Orange is broken, failed or dangerous. Yellow is "needs your input". Cyan is your selection, a match, or the primary action. Gray is everything else. No color names a category.
4. **Flat surfaces, set apart by tone.** Translucent fills and text weight show importance, not borders and shadows. Only floating layers cast a shadow.
5. **Each kind of element has a fixed shape.** Buttons are pills. Chips are rectangles with 8 px rounded corners. Containers are rectangles with 12 px rounded corners. Avatars and icon buttons are circles.
6. **Reasons are words.** A graphic shows a state. Text next to it gives the reason. Do not invent an icon or a color for a reason.
7. **Reduced modes are visible, and actions can be undone.** A degraded mode (a mode with reduced function) shows a badge. An action with significant consequences asks for confirmation in place. A completed action offers Undo.

## 2. Reading the values

- The words MUST, MUST NOT, SHOULD and MAY have the meanings in RFC 2119.
- Each rule has an ID, such as `COL-3`. Use it in reviews and commits. Section 9 lists the blocking rules, which stop a release when broken.
- **Page**: the application window. **Stage**: the one large canvas. **Chrome**: the controls that float over the stage. **Masthead**: the bar across the top. **Surface**: a filled area that holds content.
- **Additive fill**: a see-through white layer (dark theme) or black layer (light theme) over a surface. **Tonal** means it uses the additive fill. **Inverted fill**: a fill in the text color, with page-colored text on it.
- **Live edge**: the present moment. A view behind live shows a past time.
- **Type**: "500 14/20" is weight 500, 14 px text, 20 px line height. Weight 400 is regular, 500 medium, 700 bold. "mono" is the monospace family `--mono`.
- **Pixels** are logical points at scale 1.0.
- **Padding and margin** read top, right, bottom, left. Three values are top, left and right, bottom. Two values are top and bottom, then left and right. "gap 4 px 16 px" is 4 px between rows, 16 px between columns.
- **Sizes**: "64 x 36 px" is width by height. "min(460 px, 70vh)" is the smaller value; `vh` is a percent of window height.
- **Tokens**: a name such as `--text-2` is a named value. Section 10 lists them all. "white at 50%" is white at 50% opacity.

## 3. Color meanings

One table holds every color rule. Token values are in section 10.

| Rule | Color | Means | Notes |
|---|---|---|---|
| COL-1, COL-2 | Violet: `--accent`, `--progress` (gradient), `--indicator` (deeper violet) | Progress or live. It fills progress bars, the timeline scrubber and the live dot. | The one accent element of the brand mark (AST-2) is the only other use. The count badge and the live status badge use `--indicator`, because white text on `--accent` is below 4.5:1. |
| COL-3 | Yellow: `--needs-input` | Needs your input: a question, an approval, a decision, or a failure the user must deal with. | Means nothing else, on the stage or the page. |
| COL-4 | Cyan: `--primary` | The current selection, a search or filter match, the primary action, or "new since you last looked". | On the stage, cyan means only the selection or a match. "New since you last looked" is a page color. |
| COL-5 | Gray: `--text-2` (`#9AA0B4` on the stage) | Everything else, including "working" and every kind of waiting: queued, blocked, held by cap, colliding. | A waiting state names its reason in words. |
| COL-6 | Green: `--green` | A healthy or passed state, in text or in an 8 px status dot. | Not on the stage. It is not one of the five signal meanings. |
| COL-7 | Orange: `--danger`, `--danger-bg`, and on the stage `--stage-danger` | Broken, failed or dangerous. | See the thing-versus-summary rule below. |

**The thing-versus-summary rule decides yellow or orange for a failure.**

- **Orange marks the failed thing itself:** a failed run, check or gate, a worker that stopped with an error, a degraded service, a form error, and the confirm step before an action with significant consequences.
- **An indicator that sums up several items never shows orange.** It shows a failure as yellow, because a failure needs the user. This covers a row with several states (CMP-BDG-3), an aggregate indicator (CMP-BDG-4), a count pill on a collapsed parent (STG-DRAW-8) and the notification count.
- **A status dot is never orange**, except a dot for the health of one thing: a service (CMP-SET-7) or an actor (CMP-AV-4).

Two more rules apply to every color.

| ID | Rule |
|---|---|
| COL-8 | Color MUST NOT identify a category, such as a provider, a team or a type of user. Use a letter, a label or an icon. |
| COL-9 | A color MUST NOT be the only way a meaning is shown. Pair it with text, a tooltip or an accessible name (the name a screen reader announces). |

**Color-vision modes** ([decision 0070](decisions/0070-color-vision-modes-live-only-in-kgraf.md)). The app setting Color vision remaps the five meaning colors (progress, danger, needs input, selection, neutral) for protanopia, deuteranopia, tritanopia and achromatopsia. The modes are an app setting and not part of this palette. The values are in `crates/kgraf-core/src/palette.rs`. Under achromatopsia only luminance remains, so COL-9 is the only thing that works.

## 4. Type, spacing, shape

### 4.1 Type

Two families, both SIL OFL 1.1 and shipped with the app: `--font` is Instrument Sans (400, 500, 700) and `--mono` is Fragment Mono (400).

| Role | Spec |
|---|---|
| Dialog title, brand wordmark | 700 20/28 |
| Page title, detail title | 700 18/26 |
| Stage selection title | 500 18/24 |
| Section heading, panel item name, empty-state title | 500 16/22 |
| Panel and menu header | 400 16/22 |
| Search input, suggestion rows | 400 16/24 |
| Body, chip label, row title | 400 or 500 14/20 |
| Button label, size M | 500 14/36 |
| Button label, size S | 500 12/32 |
| Feed and message text, key-value text | 400 13/20 |
| Setting description, note | 400 13/18 |
| Metadata, helper text, badges | 400 or 500 12/18 |
| Stage labels | 400 or 500 12 px |
| Feed timestamps | 400 11 px |
| Stage sub-labels, count badge on stage | 400 or 500 11 px |
| Tool output in feeds | mono 400 12/20 |

| ID | Rule |
|---|---|
| TYP-1 | Weight 500 is for names, labels and buttons. Weight 700 is only for page and dialog titles, the wordmark, and the part of a search suggestion that completes what the user typed. |
| TYP-2 | Numbers that change or line up (counts, times, percentages) MUST use tabular numerals, so each digit has the same width. |
| TYP-3 | Single-line names and metadata end with an ellipsis when they do not fit. A lockup title MAY show 2 lines. Long free text wraps. |
| TYP-4 | Text on the stage MUST have a dark halo: a 3 px stroke in `rgba(0,0,0,.85)` with round joins. |
| TYP-5 | Use sentence case for labels, titles, badges and buttons. No all-capital words. |
| TYP-6 | Inline code uses `--mono` on an `--additive` fill, radius 4 px, padding 1 px 4 px. |

### 4.2 Spacing, radius and elevation

The base unit is 4 px. Prefer 2, 4, 6, 8, 12, 16, 20, 24, 32, 40, 48. A component rule that gives another value wins.

| ID | Rule |
|---|---|
| SPC-1 | Page gutters are 16 px in the masthead and 24 px in content columns. |
| SPC-2 | The gap between the primary and secondary columns is 24 px. The gap between stacked panels is 12 px. |
| SPC-3 | Rows in panels have 16 px of padding at left and right. Dense rows have 4 to 6 px above and below. |
| SPC-4 | The gap between an icon and its label is 6 px in buttons and chips, 12 px in rows with a tile, 16 px in menu items. |
| SPC-5 | Chips in a filter bar are 12 px apart. Chips and small buttons in a card are 8 px apart. |

| Radius | Use |
|---|---|
| 2 px | Timeline segment ends, user name chip, search highlight |
| 4 px | Tooltip, small badge, small thumbnail, inline code, small stage card |
| 6 px | Stage card, project tile |
| 8 px | Chip, snackbar, medium thumbnail, minimap, stage tooltip |
| 10 px | Navigation entries, option rows |
| 12 px | Stage, panels, menus, dialogs, cards, dropdowns, notes, legend |
| Pill | Buttons, count badge, segmented pill, outlined chip |
| 50% | Icon buttons, avatars, dots, scrubber knob |

| ID | Rule |
|---|---|
| ELV-1 | Layout surfaces (masthead, navigation, panels, cards) MUST NOT have a shadow. Set them apart by tone or a 1 px `--outline` border. |
| ELV-2 | Only floating layers cast a shadow. Menus and dialogs use `--shadow-menu`. |
| ELV-3 | Layer order, lowest first: navigation, masthead, dropdown, menu, dialog, snackbar, tooltip. Stage overlays stack inside the stage. |

### 4.3 Icons, motion, theming

| ID | Rule |
|---|---|
| ICO-1 | Icons are Material Symbols Outlined, weight 400. The default size is 24 px. Use 20 px in small icon buttons, 18 px in small buttons and badges, 16 px beside 12 px text. |
| ICO-2 | Icons take the current text color. Use the outlined form at rest and the filled form for the active state. Icons on the stage are white. |
| MOT-1 | Things enter more slowly than they leave: 150 to 350 ms in, 100 to 200 ms out. |
| MOT-2 | When the user asks for reduced motion, all transitions and animations MUST have zero duration, blinking stops (the dot stays solid), and scrolling is instant. |
| MOT-3 | "Working" shows as a blink. Opacity goes from 1 to 0.3 and back over 1.6 s, without end. On the stage the lowest opacity is 0.35. It applies to gray working dots and the gray aggregate indicator. |
| MOT-4 | The attention wiggle is only for "needs your input" on a collapsed control. Each cycle is 4.6 s: a 600 ms spring, then rest. It stops on hover. A new needs-input item starts it again. |
| MOT-5 | A live event on the stage is one colorless gray ring, growing from 5 to 35 px and fading over 1.2 s. |
| MOT-6 | Do not move layout the user is pointing at. Keep list order stable when an item resolves (CMP-CARD-4). |
| THM-1 | Dark is the default. Offer three choices: device theme, dark and light. |
| THM-3 | The stage, its overlays and its tooltips do not change with the theme. |
| THM-4 | Components take theme colors only from tokens. A fixed color is allowed only where this guide states it. Any other color literal in the code is a defect. |

Motion curves: `--ease-in` for things that enter or grow, `--ease-out` for things that leave, `--ease-std` for things that move in place. Typical durations: hover 100 ms; menus and tooltips 150 ms in, 100 ms out; dialogs and stage chrome 250 ms in; camera moves 500 ms; progress values 400 ms.

## 5. Layout and components

### 5.1 Layout

The window has a 56 px masthead on top, a navigation rail on the left, the primary column with the stage in the center, and a secondary column of panels on the right. The secondary column is 28% of the window width, between 360 and 402 px.

| ID | Rule |
|---|---|
| LAY-1 | The window does not scroll. Each panel and list scrolls on its own. |
| LAY-3 | The stage takes all free height in the primary column. Its minimum height is 220 px. |
| LAY-4 | The masthead has padding 0 16 px. At the start: menu button, brand mark, context label. In the center: search, at most 732 px wide. At the end: status controls and the notification button. |
| LAY-5 | The navigation rail is 72 px wide collapsed and 240 px expanded. |
| LAY-6 | The secondary column holds at most two panels: context above, a live feed below, split 3:2. |
| LAY-7 | Below the stage: the title row, then pending action cards, then the composer. |
| LAY-8 | Below 1360 px window width, show the short form of long masthead labels and reduce the search margin to 12 px. |
| LAY-9 | When content above or below the stage changes height, fit the stage to its new space in the next frame. |

### 5.2 Components

Only rules the shell relies on are listed. Hover is `--additive`, and pressed is one step stronger (INT-2).

**Buttons and chips**

| ID | Rule |
|---|---|
| CMP-BTN-1 | A region has at most one primary button (`--primary` fill). |
| CMP-BTN-2 | In a group of choices, the recommended or safest option is filled (`--inverted`). The others are tonal. |
| CMP-BTN-3 | A busy button is disabled and shows an 18 px spinner and an -ing verb ("Thinking"). |
| CMP-BTN-4 | Labels are verbs or verb phrases, with no final punctuation. Size M is 36 px high, size S is 32 px, both pills. |
| CMP-IBTN-1 | An icon button is a 40 px circle (small: 32 px) with no fill at rest. |
| CMP-IBTN-2 | Every icon button MUST have an accessible name and a tooltip that names the action and its shortcut. |
| CMP-IBTN-3 | A count badge sits at the button's top right, at least 20 px square, `--indicator` fill, white text. The count includes failures that need the user. Hide it at 0. |
| CMP-CHIP-1 | A chip is 32 px high, radius 8 px, padding 0 12 px, 500 14/20. Inactive: `--additive`. Active: `--inverted` with `--text-inv` text. |
| CMP-CHIP-2 | In a filter bar one chip is selected at a time. The first is "All". Selecting a chip highlights its matches on the stage and fits the camera to them. |
| CMP-CHIP-5 | An outlined info chip (model, mode, context) is 28 px high, 1 px `--outline` border, 500 12/18 `--text-2`. It is not interactive. |

**Status**

| ID | Rule |
|---|---|
| CMP-STAT-2 | The run toggle shows the current state, not the action: a pause icon and "running", or a play icon and "paused". The tooltip names the action. |
| CMP-STAT-3 | A mode badge is 32 px high, radius 8 px, `--additive` fill. In a degraded mode it uses `--danger-bg` fill with `--danger` text and icon, and its tooltip says what is degraded. |
| CMP-BDG-1 | A status badge is 22 px high, radius 4 px, 500 12/18. Neutral: `--additive` and `--text-2`. Danger: `--danger-bg` and `--danger`. OK: `--green` text. Live: `--indicator` and white. |
| CMP-BDG-2 | A status dot is 8 px. Yellow: needs input, including a failure that needs the user. Cyan: new results since you last looked. Gray and blinking: working. No dot: idle. |
| CMP-BDG-3 | A row with several states shows one dot, for the most urgent: yellow first, then cyan, then gray. A failure counts as needs input. |
| CMP-BDG-4 | An aggregate indicator (for example, the tint of a collapsed menu button) follows CMP-BDG-3. It is yellow if any item needs input or failed; else cyan if any has new results; else gray and blinking if any is working; else untinted. Its tooltip and accessible name give kinds and counts in words, such as "Project A: 1 failed run, 2 questions". While the list it stands for is open, it is neutral. |

**Avatars**

| ID | Rule |
|---|---|
| CMP-AV-1 | Avatars are circles of 24, 36, 40 or 48 px. |
| CMP-AV-2 | Actors (agents, services, bots) use `--outline-opaque` fill and a `--text` letter naming the kind of actor. Color never does (COL-8). |
| CMP-AV-3 | The user's avatar and name chip use `--inverted` fill with `--text-inv`. This is a contrast treatment, not a meaning, so yellow keeps its one meaning. |
| CMP-AV-4 | A state dot sits at an actor avatar's bottom right: 34% of the diameter, at least 8 px, with a 2 px ring in the surface color. Orange: the actor stopped with an error. Gray and blinking: working. Yellow: needs input. No dot: idle. |
| CMP-AV-6 | A merge-conflict resolver is an actor, marked so it is not mistaken for one: its state dot is a rounded square, its letter is "R", and its header and row carry the label "Resolver". Dot colors follow CMP-AV-4. |

**Navigation, panels, rows**

| ID | Rule |
|---|---|
| CMP-NAV-3 | Collapsed, the current entry shows only its filled icon. Expanded, it has the `--additive` fill, weight 500 and the filled icon. |
| CMP-NAV-5 | A list of items the user switches between (projects) goes at the top of the expanded rail. Each row is at least 48 px high, with a 28 px tile, the name, a status line and a status dot (CMP-BDG-2). Switching changes only the view. |
| CMP-NAV-6 | The menu button opens and closes the expanded rail. While collapsed, it carries the aggregate indicator (CMP-BDG-4). |
| CMP-NAV-7 | The navigation rail is the only way into settings. No menu header, status control or badge opens settings. A control that shows a setting's value only shows it. |
| CMP-PNL-1 | A panel has a 1 px `--outline` border, radius 12 px and `--base` fill. Its content is cut at the corners. |
| CMP-ROW-2 | A message has padding 4 px 16 px, 13/20 text and a 24 px avatar. The timestamp (11 px `--text-3`), the author (weight 500) and the text run on as one line. |
| CMP-ROW-3 | A live feed lists the newest event first. Each row has a 52 px timestamp column, a 24 px avatar and the text, and gives the actor name to screen readers. |
| CMP-ROW-5 | When the view shows the past, the feed header reads "Viewing HH:MM:SS" with a gray dot, the current row has the `--suggested` fill, and later rows show at 38% opacity. At the live edge it reads "Live" with a violet dot. |

**Pending action cards** hold a request that needs the user: an approval or a question.

| ID | Rule |
|---|---|
| CMP-CARD-1 | A card has `--additive` fill, radius 12 px, padding 10 px 14 px 12 px. Up to two sit side by side with a 12 px gap. |
| CMP-CARD-2 | The header row has a 24 px avatar, `@name` in 500 13/18, the time, and a pin label. The actions row uses size S buttons 8 px apart. |
| CMP-CARD-3 | An approval offers a filled recommended choice and tonal alternatives. Choosing one changes the actions row to a confirm step: a 16 px warning icon and a one-line consequence in `--danger`, a primary Confirm button and a text Cancel button. Focus moves to Confirm. Cancel restores the choices and the focus. |
| CMP-CARD-4 | Cards keep a stable order. Resolving one MUST NOT move another under the pointer. |
| CMP-CARD-5 | When the user opens a card from elsewhere, scroll it into view, focus its first enabled button and flash it: a 2 px `--primary` ring and the `--suggested` fill for 1.4 s. |
| CMP-CARD-7 | After an approval, a snackbar states the result and offers Undo. An answered question shows the answer in the conversation instead. |

**Fields, menus, dialogs**

| ID | Rule |
|---|---|
| CMP-SRCH-1 | The search field is two joined parts, both 40 px high. The input has `--search-bg` fill, a 1 px `--search-border` border and a `--text-2` placeholder. The button is 64 x 40 px. |
| CMP-FLD-2 | A text area grows from 24 px to 120 px high, then scrolls. The placeholder names the keys to use. |
| CMP-FLD-3 | Enter sends. Shift+Enter adds a line. Do not send while an input method editor is composing text. |
| CMP-FLD-4 | Helper and error text is 400 12/18 with a 16 px icon. Error text is `--danger` and is announced at once. Sending an empty message shows an error and never silently does nothing. |
| CMP-MENU-1 | A menu has `--menu` fill, radius 12 px, `--shadow-menu`, and a minimum width of 220 px. It opens directly below its trigger. Down and Up move between items. Escape closes it and returns focus to the trigger. |
| CMP-TIP-4 | A tooltip shows after 400 ms of hover, at once on keyboard focus, and hides on press, pointer leave and focus leave. Tooltip text names the action with its shortcut: "Zoom in (+)". |
| CMP-SNK-1 | A snackbar sits at the bottom left, `--inverted` fill, with at most one action (usually Undo). One shows at a time. It stays 5 s and pauses under the pointer. |
| CMP-DLG-1 | A modal dialog has `--scrim` behind it, `--menu` fill, radius 12 px and `--shadow-menu`. Its size is min(760 px, 100vw - 48 px) by min(600 px, 100vh - 48 px). |
| CMP-DLG-2 | The header holds the title (700 20/28), a description (400 14/20 `--text-2`) and a close icon button. |
| CMP-DLG-3 | A dialog with sections has a 200 px vertical list of 40 px tabs (the selected one has `--additive` fill and weight 500) and a scrolling content area. |
| CMP-DLG-4 | The footer is right-aligned with one text-primary "Done" button. Settings take effect at once, so there is no Save. |
| CMP-DLG-5 | Escape or a click on the scrim closes the dialog. Focus the selected tab on open, and return focus to the opener on close. |
| CMP-DLG-6 | A settings dialog holds configuration only. It MUST NOT show work state or repeat what the stage shows. |

**Settings controls**

| ID | Rule |
|---|---|
| CMP-SET-1 | A setting row has padding 16 px 0 and a 1 px `--outline` line below (not on the last row). The label is 500 14/20. The current value sits at the right in 500 14/20 with tabular numerals. The description is 400 13/18 `--text-2`, 4 px below. |
| CMP-SET-2 | A slider has a 24 px hit area and a 4 px track: `--primary` up to the value, `--outline` after. The thumb is a 14 px `--primary` circle. Tick labels sit below in 12/18 `--text-2`. |
| CMP-SET-3 | A recommended value shows as a 2 x 18 px mark on the track with a legend "Recommended: value". When the value moves away, a size S text-primary button "Return to recommendation" appears. If the recommendation was not measured, the legend says so. |
| CMP-SET-4 | A switch is 36 x 20 px. Track off: `--text-dis` at 60%. Track on: `--primary` at 50%. Thumb off: `#EDEFF7`. Thumb on: `--primary`. It has the role `switch` and a checked state. |
| CMP-SET-5 | A single-choice list has 40 px rows, radius 10 px, a check column and a leading icon. Up and Down move between rows. |
| CMP-SET-6 | A note has `--additive` fill, radius 12 px, padding 12 px 16 px, and 400 13/18 `--text-2` text. It states a fixed behavior and is not a warning. |
| CMP-SET-7 | A status list (providers, services) has rows with an 8 px dot, the name in 500 14/20, the state in 500 12/18, and a detail line in `--text-2`. Dot and state are `--green` when healthy and `--danger` when degraded or failed. |

**Empty states.** CMP-EMP-1: centered, a 72 px `--additive` ring with a 36 px icon, a 500 16/22 title and 400 14/20 `--text-2` text, at most 300 px wide. CMP-EMP-2: the title says the state in plain words ("Nothing needs you right now"), and the text says what will show up and from where. CMP-EMP-3: a no-results state repeats the query and suggests one next step.

## 6. Stage drawing rules

The stage palette is fixed in every theme. Its values are the `STAGE` literals in `crates/kgraf-core/src/palette.rs`; the shell and the galaxy both read them.

### 6.1 Frame and overlays

| ID | Rule |
|---|---|
| STG-1 | The stage is `--stage-bg` in every theme, radius 12 px, content cut at its edge. All overlay text and icons on it are white. |
| STG-3 | The selection title sits at the top left, 12 px in from the edges: a 40 px avatar, the title (500 18/24), metadata saying how to clear the selection ("Esc clears") and a clear button. |
| STG-5 | The control bar is 48 px high and 12 px in from each side, with the timeline scrubber above it. It holds one group of view controls at the right: overlay toggles, zoom out, zoom in, fit. There are no playback controls. |
| STG-7 | The legend sits at the top right: 220 px wide, `--stage-popup` fill, radius 12 px. A chevron collapses it. It collapses itself when it would overlap the minimap, and a user's choice wins. |
| STG-8 | The minimap is 172 px wide at the bottom right, radius 8 px. It shows all nodes as 1 px dots, actors as 4 px dots and the current view as a frame. A click or drag moves the view. It moves down when the chrome hides. |
| STG-HIDE-1 to 5 | Pointer or focus movement in the stage shows the chrome. It hides 2.6 s after the last activity, only if the view is at the live edge, no drag runs, focus is not on a stage control and the pointer is not over the control bar. It hides 0.4 s after the pointer leaves. Behind live, it stays visible. |

### 6.2 Timeline scrubber

| ID | Rule |
|---|---|
| STG-TL-1 | The hit area is 15 px high. The bar is 3 px high, and 5 px on hover, drag or focus. |
| STG-TL-2 | The bar is split into named segments (chapters). The part after the viewed time is white at 20%. The part up to it is `--progress`, drawn as one gradient across all segments. |
| STG-TL-3 | The knob is a 13 px `--accent` circle, hidden at rest and visible on hover, drag or focus. It is always visible when the view is behind live. |
| STG-TL-7 | To return to live, click or drag to the right end, or press End. Left and Right step one event, and Home goes to the start. The timeline has the role `slider`. |

### 6.3 Drawing

| ID | Rule |
|---|---|
| STG-DRAW-1 | Each population (kind of object) has one shape: dots for items, rounded cards for tasks, circles with a letter for actors. |
| STG-DRAW-2 | Item dots are gray. Their radius grows with the square root of their connections. A search or filter match is cyan. |
| STG-DRAW-3 | With a highlight active, everything outside it draws at 15% opacity. Relation arrows and count pills outside draw at 40%, parent-child links at 35%. |
| STG-DRAW-4 | Hover: a 1 px white ring at 55 to 60%, 3 to 5 px outside the shape. Selection: a 2 px cyan ring, and its direct relations in cyan at 55%. A match: a 1.5 px cyan ring. |
| STG-DRAW-5 | A task card is `#171A26` with a 1 px border in white at 34% and radius 6 px (4 px small). A progress strip runs across the bottom, 4 px high (3 px small), with a white-at-22% track and a `--progress` fill. A done task shows a white check. A collapsed parent shows "+N" at its top left. |
| STG-DRAW-6 | A waiting item draws at 50% opacity. A sub-label in words gives the reason ("blocked by X", "queued"). A thin gray arrow points to its cause. |
| STG-DRAW-7 | An actor is a disk of 12 px radius, `#1E2233` fill, a 1.2 px border in white at 60% and a 500 12 px letter. A state dot sits at its lower right (CMP-AV-4). A relation the actor holds is a dashed gray line, solid cyan when the actor is selected. |
| STG-DRAW-8 | A needs-input item shows a yellow count pill at the top right of its card. A hidden child, and a failed child, add to the pill on the nearest visible ancestor. Clicking the pill opens the matching cards (CMP-CARD-5). |
| STG-DRAW-9 | Labels are 12 px with the text halo, placed in priority order. A label that fits nowhere is not drawn. Hovered and selected labels always draw. A sub-label is 11 px, white at 62%. |
| STG-DRAW-10 | Group labels are 400 12 px. Below 2 x the fit scale they show the short name, above it the full name (STG-CAM-1). |
| STG-DRAW-11 | A failed or broken item is orange on the item itself. A failed task card keeps its shape. Its strip stops where it failed, and it gets a 1.5 px `--stage-danger` ring and an 18 px pill at its top left with an error icon. A sub-label gives the reason, such as "failed: 3 tests". A failed actor gets the orange dot. A failed item hidden in a collapsed parent counts in the parent's yellow pill instead (STG-DRAW-8). |

### 6.4 Camera and performance

| ID | Rule |
|---|---|
| STG-CAM-1 | Camera scale is screen pixels per world unit. The fit scale is the scale after "Fit", which frames all content clear of the overlays. The zoom range runs from 0.6 x the fit scale to a camera scale of 5. |
| STG-CAM-2 | Wheel zoom is continuous and keeps the point under the pointer fixed. Buttons and keys zoom 1.4 x per step. |
| STG-CAM-3 | Selecting an object from outside the stage moves the camera to it in 500 ms. A resize refits only if the view was at fit. Keys: arrows pan, `+` and `-` zoom, `0` fits, Escape clears. |
| STG-PERF-1 | Redraw only when something changes. A blinking state needs about 15 redraws per second, not 60. |
| STG-PERF-4 | A software-rendering mode says so in a mode badge (CMP-STAT-3). If the mode leaves out a costly effect, the tooltip names it. |

## 7. Interaction, content and accessibility

| ID | Rule |
|---|---|
| INT-1 | The focus ring is a 2 px solid `--text` line, offset 2 px outside the element (inside for filled or clipped containers). On the stage it is white. It shows for keyboard focus. |
| INT-2 | Hover and pressed states change the fill. They add no borders or shadows. |
| INT-3 | Escape works in steps. Each press closes the top layer: menu, tooltip, selection, filter, then search. |
| INT-4 | Single-key shortcuts MUST NOT fire while the user types in a field or holds Ctrl, Cmd or Alt. |
| INT-5 | The single-key shortcuts are `/` for search, `m` for the minimap, and `+` `-` `0` for zoom and fit. No playback keys. |
| INT-7 | The minimum target is 24 x 24 px. Buttons, chips and icon buttons are 32 px high or more. A thin target that spans its row, such as the scrubber, MAY be shorter. |
| INT-8 | An action with significant consequences has a two-step confirm in place (CMP-CARD-3). A completed action has Undo. Do not use a modal dialog to confirm. |
| TXT-1 | Write short, direct sentences in sentence case. No exclamation marks. |
| TXT-2 | State a status in words, with its reason: "Held by cap", "Blocked by w2", "Idle". |
| TXT-3 | Times are 24-hour `HH:MM:SS`. A count agrees with its noun: "1 agent", "3 agents". |
| TXT-5 | A tooltip says what a control does, not what it is: "Pause the scheduler". |
| TXT-6 | A disabled or degraded state says why, and what to do about it. |
| TXT-7 | Do not claim a benefit the product cannot measure. |
| A11Y-1 | Text MUST meet WCAG 2.2 AA contrast on its surface in both themes: 4.5:1, or 3:1 for large text. Non-text parts that carry meaning MUST meet 3:1. Disabled text and decorative borders are exempt. Tests check only the pairs named under this table. Measure any other pair of tokens before you use it. |
| A11Y-2 | Every control has an accessible name. A control that shows only an icon gets its name from its tooltip text. |
| A11Y-3 | Use the native role for each control: button, switch, slider, tab, radio group, menu, dialog. |
| A11Y-4 | Live feeds are announced politely. Form errors are announced at once. |
| A11Y-5 | The stage canvas can take focus. Its accessible name lists its keys. |
| A11Y-6 | A state shown only by color (dots, rings) also appears in the accessible name or the tooltip. |

**What is checked for contrast.** The five semantic colors meet WCAG 2.2 AA against their surface in every color-vision mode: 4.5:1 on a page, 3:1 on the stage, and 3:1 for progress, which only draws dots, bars and the scrubber knob. The test is `vision_modes_meet_wcag_aa` in `crates/kgraf-core/src/palette.rs`. The shell tests more pairs in `vision_modes_extended_wcag_pairs` (`crates/kgraf-shell/src/theme.rs`): danger on a badge, a card and a menu, and selection on the page. Other token pairs, such as body text on each surface, have no automated check. Apply A11Y-1 when you add one.

## 8. Applying this to the native shell

kGraf's shell is a native Rust app built with egui and `wgpu`. It has no webview and no CSS ([decision 0054](decisions/0054-a-native-rust-shell-that-runs-without-a-gpu.md)). Some rules above use web words. This table gives the native meaning.

| Guide term | In the shell |
|---|---|
| CSS pixel | A logical point at scale 1.0 |
| Token (`--text-2`) | A named constant in `crates/kgraf-shell/src/theme.rs`, fed by `kgraf_core::palette`. The galaxy reads the same palette |
| `data-theme`, `color-scheme` | The Appearance setting: device, dark or light, mapped to egui's theme preference |
| `prefers-reduced-motion` | The Reduce motion switch in Settings. The shell does not read an operating system flag |
| `:focus-visible` | egui keyboard focus |
| ARIA role, label, live region | An AccessKit node. egui builds it from each widget's type and label (`widget_info`). Tests read it through the accessibility tree |
| Tabular numerals | Digits baked into the shipped font files, because egui applies no OpenType features |
| z-index | egui layer order |
| `text-shadow` | A halo drawn behind the text on the canvas |

**Normative rules.** Sections 1, 3, 7 and 9 bind every surface. Sections 4 to 6 bind in their stated values, except where a value assumes a browser, such as a stylesheet property. For those, apply the intent. The color rules (COL-1 to COL-9), CMP-DLG-6 and CMP-NAV-7 are checked in review.

**Assets.**

| ID | Rule |
|---|---|
| AST-1 | The product ships all fonts and icons in its own source tree and makes no network request for assets. |
| AST-2 | Each product has its own original brand mark: one color (`--text`) with at most one `--accent` element. |
| AST-3 | The source, version, date and license of each asset are in `crates/kgraf-shell/assets/SOURCES.md`. |
| AST-4 | Each font keeps its real family name. |

## 9. Blocking rules

A reviewer checks these first. A breach of any of them is a blocking defect.

`COL-1`, `COL-3`, `COL-8`, `COL-9`, `LAY-1`, `MOT-2`, `CMP-BTN-1`, `CMP-IBTN-2`, `CMP-CARD-3`, `CMP-CARD-4`, `CMP-DLG-6`, `STG-1`, `THM-3`, `THM-4`, `INT-4`, `A11Y-2`, `AST-1`.

## 10. Token block

Each token is a named constant in the shell. The block is written as CSS because that is the shortest exact form.

```css
:root {
  /* Type */
  --font: "Instrument Sans", "Helvetica Neue", Arial, sans-serif;
  --mono: "Fragment Mono", ui-monospace, Menlo, monospace;

  /* Layout */
  --masthead-h: 56px;  --nav-mini: 72px;  --nav-full: 240px;
  --secondary-w: clamp(360px, 28vw, 402px);

  /* Fixed colors (both themes) */
  --accent: #8B5CF6;      /* live dot, scrubber knob, brand accent */
  --indicator: #7C3AED;   /* count badge, live status badge */
  --progress: linear-gradient(90deg, #8B5CF6 70%, #C084FC);
  --stage-bg: #07080F;
  --stage-popup: rgba(22,25,38,.92);   /* stage tooltips, legend, popup buttons */
  --stage-danger: #FF7A33;             /* failed or broken items on the stage */

  /* Motion */
  --ease-in: cubic-bezier(0,0,.2,1);
  --ease-out: cubic-bezier(.4,0,1,1);
  --ease-std: cubic-bezier(.2,0,0,1);
  --ease-search: cubic-bezier(.2,0,.6,1);
  --ease-spring: cubic-bezier(.34,1.56,.64,1);   /* the attention wiggle only */
}

:root[data-theme="dark"] {
  --base: #0D0F17;          /* page and panel background */
  --raised: #171A26;        /* dropdown lists, suggestions */
  --menu: #1D2130;          /* menus, dialogs */
  --inverted: #EDEFF7;      --inverted-hover: #D3D7E4;
  --additive: rgba(255,255,255,.1);   --additive-soft: rgba(255,255,255,.05);
  --chip-hover: rgba(255,255,255,.2); --outline: rgba(255,255,255,.2);
  --outline-opaque: #2E3345;
  --text: #EDEFF7;  --text-2: #9AA0B4;  --text-3: rgba(255,255,255,.54);
  --text-dis: #6B7186;  --text-inv: #0D0F17;
  --primary: #22D3EE;  --primary-hover: #67E8F9;  --primary-inv: #0E7490;
  --suggested: #10303B;     /* highlighted row, flashing card */
  --danger: #FB923C;  --danger-bg: rgba(251,146,60,.16);
  --needs-input: #FACC15;  --green: #34D399;
  --search-bg: #0A0C13;  --search-border: #2A2F40;  --search-btn: rgba(255,255,255,.06);
  --search-btn-border: #2A2F40;  --search-focus: #0891B2;
  --tooltip: rgba(46,51,69,.94);  --scrim: rgba(5,6,12,.6);  --scrollbar: #4A5068;
  --shadow-menu: 0 4px 32px rgba(0,0,0,.35);
}

:root[data-theme="light"] {
  --base: #F7F8FC;  --raised: #FFFFFF;  --menu: #FFFFFF;
  --inverted: #10131C;  --inverted-hover: #262A38;
  --additive: rgba(16,19,28,.05);  --additive-soft: rgba(16,19,28,.03);
  --chip-hover: rgba(16,19,28,.1);  --outline: rgba(16,19,28,.12);
  --outline-opaque: #E1E4EE;
  --text: #10131C;  --text-2: #5A6072;  --text-3: rgba(16,19,28,.6);
  --text-dis: #8A90A2;  --text-inv: #FFFFFF;
  --primary: #0E7490;  --primary-hover: #155E75;  --primary-inv: #22D3EE;
  --suggested: #DDF6FB;
  --danger: #B83C0B;  --danger-bg: #FFEDE3;
  --needs-input: #A16207;  --green: #047857;
  --search-bg: #FFFFFF;  --search-border: #C9CEDC;  --search-btn: #EEF0F6;
  --search-btn-border: #C9CEDC;  --search-focus: #0E7490;
  --tooltip: rgba(46,51,69,.94);  --scrim: rgba(16,19,28,.4);  --scrollbar: #A3A9BA;
  --shadow-menu: 0 4px 32px rgba(16,19,28,.12);
}
```
