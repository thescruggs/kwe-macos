# Alpha M4j — pointer button protocol

M4j extends the normalized pointer transport with explicit primary, secondary,
and middle button down/up events. Button phases require a button value, while
hover/move/leave events remain buttonless. This preserves bounded coordinates
and gives future interaction-mode workers enough information to map clicks
without swallowing Plasma gestures by default.
