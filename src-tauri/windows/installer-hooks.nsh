; Tauri's stock NSIS template always inserts a destination-directory page.
; Heya is a current-user application with a fixed install location, so replace
; that one page macro with a no-op while leaving the rest of Tauri's installer
; and updater template unchanged.
!macroundef MUI_PAGE_DIRECTORY
!macro MUI_PAGE_DIRECTORY
!macroend
