## 2025-05-14 - Icon-only Action Button Accessibility Pattern
**Learning:** Icon-only buttons often lack descriptive text for screen readers. Implementing a hierarchical fallback for accessibility labels (explicit label -> visual label -> tooltip) ensures that interactive elements always have some form of announcement while allowing developers to provide high-context overrides.
**Action:** Always provide an explicit accessibility label for icon-only ActionButtons using the `.with_accessibility_label()` builder method, especially when the action is context-dependent (e.g., delete buttons).
