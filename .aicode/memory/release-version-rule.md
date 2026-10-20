---
name: release-version-rule
description: AppOptR 发布版本规则：每次更新必改版本号并同步版本信息；大更新改大版本号，小更新小幅提升
---
# AppOptR 发布版本规则（用户约定）

- 每次发布更新**必须**提升版本号，并同步版本信息：
  - `module/module.prop` 的 `version` / `versionCode`
  - `modules_update/changelog.md` 新增对应版本条目
  - `README.md`（当前不含版本字段，若未来加入需同步）
- 大更新（新功能、规则语法变更、用户可见行为变化、架构调整）→ 改大版本号（如 2.2.x → 2.3.0）
- 小更新（bug 修复、内部健壮性加固、无用户可见行为变化）→ 小幅提升 patch（如 2.2.6 → 2.2.7），`versionCode` +1
- 提升操作用 `scripts/bump-version.sh`：无参 = versionCode+1；带参（如 `2.2.7`）= version 改为该值且 versionCode+1
- 改完重跑 `scripts/package.sh`（源码未变时无需重编译）重新打包 zip（zip 文件名含版本号）