## 2025-05-14 - [Accessibility Tiered Fallback for ActionButton]
**Learning:** Foundational UI components like `ActionButton` often lack explicit accessibility metadata. Implementing a tiered fallback (explicit label -> visual label -> tooltip) ensures that even icon-only buttons are screen-reader friendly without requiring every instance to be manually updated immediately.
**Action:** Always implement `View::accessibility_contents` and `View::accessibility_data` for new UI components, and use tooltip text as a final fallback for labels when appropriate.
