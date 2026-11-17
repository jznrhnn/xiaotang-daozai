岛仔 Daozai 的 AI 探索版 v0.7.1，基于上游 v0.6.65，功能优先。更看重稳定性时，建议使用[原版](https://github.com/Renly1994/xiaotang-daozai/releases/tag/v0.6.65)。

## 本次更新

- 合入上游的天气、健康提醒、独立常驻时钟和音乐适配。天气与 AI 任务各有独立页面，升级后会迁移已有的可见性设置和页面顺序。
- 邮箱可在岛内搜索和复制正文，支持上游的 HTML 邮件排版。新验证码可直接弹出，显示时长可在邮箱设置中调整。
- 云同步可选同步邮箱授权码。允许本机缺少授权码时恢复，恢复中途失败会回滚。
- 手机交互支持钉钉个人授权与应用机器人私聊通知，可回复并继续本机 Codex 原任务。发送 `/tasks` 查看可回复的任务。钉钉收发组件随安装包和免安装包提供，个人授权在「设置 → 手机交互」完成。

## 下载

- 安装版 `Daozai-Setup-v0.7.1-x64.exe`
- 免安装版 `Daozai-v0.7.1-windows-x64.zip`，完整解压后运行 `daozai.exe`
- 校验文件 `SHA256SUMS.txt`

请先退出旧版再更新。新版沿用 `%APPDATA%\Daozai` 数据目录。安装包和免安装包均为 Windows x64。完整变更见 [更新记录](https://github.com/jznrhnn/xiaotang-daozai/blob/v0.7.1/CHANGELOG.md)。
