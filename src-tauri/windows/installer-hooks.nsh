; Tauri's stock NSIS template always inserts a destination-directory page.
; Heya is a current-user application with a fixed install location, so replace
; that one page macro with a no-op while leaving the rest of Tauri's installer
; and updater template unchanged.
!macroundef MUI_PAGE_DIRECTORY
!macro MUI_PAGE_DIRECTORY
  ; MUI normally clears page callbacks after expanding its page macro. Since
  ; this replacement deliberately emits no page, clear them here so they do
  ; not leak into the following Start Menu page.
  !ifdef MUI_PAGE_CUSTOMFUNCTION_PRE
    !undef MUI_PAGE_CUSTOMFUNCTION_PRE
  !endif
  !ifdef MUI_PAGE_CUSTOMFUNCTION_SHOW
    !undef MUI_PAGE_CUSTOMFUNCTION_SHOW
  !endif
  !ifdef MUI_PAGE_CUSTOMFUNCTION_LEAVE
    !undef MUI_PAGE_CUSTOMFUNCTION_LEAVE
  !endif
!macroend
