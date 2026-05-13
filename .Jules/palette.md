## 2025-05-14 - Tiered Accessibility Labels for Common Components
**Learning:** Common interactive components like `ActionButton` should implement a tiered accessibility label fallback (explicit `accessibility_label` > visual `label` > `tooltip`) to provide meaningful metadata to screen readers without requiring manual effort for every instance.
**Action:** Always implement this fallback pattern when building or updating shared UI components to ensure baseline accessibility.
