## 2026-05-19 - Enhanced ActionButton Accessibility
**Learning:** Core UI components like buttons, especially when icon-only, often lack descriptive labels for screen readers even if they have tooltips. Implementing a tiered fallback logic (explicit label > visual label > tooltip) at the component level ensures broad accessibility across the app.
**Action:** Always implement accessibility fallback patterns in base UI components to provide a safe default for assistive technologies.
