## 2025-05-14 - Tiered Accessibility Labels for Generic UI Components

**Learning:** When adding accessibility support to highly reusable UI components (like ActionButton), a tiered fallback system (Explicit Label > Visual Label > Tooltip) ensures that elements are always accessible by default while still allowing precise overrides for complex cases.

**Action:** Always implement tiered fallbacks in core design system components to provide a "safe" baseline for accessibility across the entire application.
