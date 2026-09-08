; LoopMaster 自定义 NSIS hooks（Tauri 2 `installerHooks`）。
;
; 问题：每次升级/重装时桌面快捷方式会累积（旧 .lnk 未删，Windows 对重名
; 再加 "- Shortcut" 后缀）。这里在安装器创建快捷方式之前删除历史遗留，
; 保证装完始终只有一个桌面快捷方式。
;
; 快捷方式名沿用默认 productName「LoopMaster」；同时覆盖用户桌面与公共桌面、
; 以及 Windows 自动加后缀的变体，避免漏删。

!macro NSIS_HOOK_PREINSTALL
  ; 清理用户桌面历史遗留/重复快捷方式
  Delete "$DESKTOP\LoopMaster.lnk"
  Delete "$DESKTOP\LoopMaster - Shortcut.lnk"
  ; 清理公共桌面（perMachine 安装可能落在公共桌面）
  Delete "$PUBLICDESKTOP\LoopMaster.lnk"
  Delete "$PUBLICDESKTOP\LoopMaster - Shortcut.lnk"
!macroend
