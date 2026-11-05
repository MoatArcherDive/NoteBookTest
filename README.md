<h1 align="center">quiet.md</h1>

<p align="center">
  <b>一个又快又安静的本地笔记应用。</b><br>
  Rust + GPUI 原生绘制，笔记全部存在你的电脑上，兼容飞书文档的 DocxXML 格式。
</p>

<p align="center">
  <a href="https://github.com/s1dashu/quiet-md/releases/latest"><img src="https://img.shields.io/github/v/release/s1dashu/quiet-md?label=release&color=2f6feb" alt="最新版本"></a>
  <a href="https://github.com/s1dashu/quiet-md/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/s1dashu/quiet-md/ci.yml?branch=main&label=CI" alt="CI 状态"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Windows-lightgrey" alt="支持 macOS 和 Windows">
  <img src="https://img.shields.io/badge/built%20with-Rust%20%2B%20GPUI-dea584?logo=rust" alt="使用 Rust 和 GPUI 构建">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/s1dashu/quiet-md?color=green" alt="MIT 许可"></a>
  <img src="https://img.shields.io/badge/status-beta-orange" alt="Beta">
</p>

<p align="center">
  <a href="https://download.quiet.md/latest/quiet-md-macos-arm64.zip"><img src="docs/images/download-macos.svg" height="56" alt="下载 macOS 版"></a>
  &nbsp;
  <a href="https://download.quiet.md/latest/quiet-md-windows-x64-setup.exe"><img src="docs/images/download-windows.svg" height="56" alt="下载 Windows 版"></a>
</p>

<p align="center">
  <a href="https://quiet.md"><b>官网</b></a> ·
  <a href="#下载"><b>下载</b></a> ·
  <a href="#性能">性能</a> ·
  <a href="#功能">功能</a> ·
  <a href="CONTRIBUTING.md">参与开发</a>
</p>

<p align="center">
  <img src="docs/images/home.png" width="100%" alt="quiet.md 的主页：左侧是文档树，右侧按网格排列所有笔记。">
</p>

> [!NOTE]
> quiet.md 目前是 **Beta 版本**，还在快速迭代。遇到 bug 或有想法，欢迎[提 issue](https://github.com/s1dashu/quiet-md/issues)，更欢迎直接提 PR，一起把它打磨好。

## 为什么是 quiet.md

**⚡ 快，原生的那种快。** 本地编辑界面由 Rust 和 [GPUI](https://www.gpui.rs/) 直接用 GPU 绘制，不使用 Electron；在线文档使用系统 WebView。百万字文档实测中，输入、删除和滚动的帧处理 P95 均低于 6 ms。

**🔒 本地优先，文件归你。** 本地笔记无需账号或云端服务。文档库就是电脑上的一个普通文件夹，每篇笔记是一个 XML 文件。用 Git、iCloud、坚果云或任何同步工具管理都可以；使用飞书链接和同步功能时需要联网并授权飞书账号。

**📝 兼容飞书 DocxXML，完整的富文本编辑器。** 笔记直接以飞书文档的 DocxXML 格式保存，从飞书复制粘贴、导入导出都能保留结构。标题、待办、表格、分栏、高亮块、代码、公式、图片和附件，写文档常用的块都有。

## 下载

<p align="center">
  <a href="https://download.quiet.md/latest/quiet-md-macos-arm64.zip"><img src="docs/images/download-macos.svg" height="56" alt="下载 macOS 版"></a>
  &nbsp;
  <a href="https://download.quiet.md/latest/quiet-md-windows-x64-setup.exe"><img src="docs/images/download-windows.svg" height="56" alt="下载 Windows 版"></a>
</p>

| 平台 | 系统要求 |
|---|---|
| **macOS**（Apple Silicon） | macOS 13 及以上 |
| **Windows**（x64） | Windows 10 2004 及以上 |

- **macOS**：解压后把 `quiet.md.app` 拖进「应用程序」。安装包已签名并通过 Apple 公证。
- **Windows**：运行安装程序，默认只装到当前用户目录，卸载不会删除笔记。如果首次运行出现 SmartScreen 提示，点「更多信息 → 仍要运行」即可。

也可以在官网 [quiet.md](https://quiet.md) 下载。所有版本和 SHA-256 校验值见 [GitHub Releases](https://github.com/s1dashu/quiet-md/releases)。Intel Mac 暂无安装包，可以[从源码构建](CONTRIBUTING.md#从源码构建)。

首次启动会在空文档库里放一套「开始指南」，十分钟带你过一遍主要功能；已有的笔记不会被覆盖。

## 性能

在一篇 **100 万字**、549 章的混合文档里连续操作 100 次，统计每帧处理耗时：

| 操作 | P50 | P95 |
|---|---:|---:|
| ⌨️ 连续输入 | **5.06 ms** | 5.78 ms |
| ⌫ 连续退格 | **4.99 ms** | 5.55 ms |
| 🖱️ 连续滚动 | **2.08 ms** | 4.14 ms |

120 Hz 屏幕每帧的预算是 8.3 ms，百万字文档下的 P95 也留有余量。

| | |
|---|---|
| macOS 安装包 | 约 18 MB |
| Windows 安装包 | 约 14 MB |
| 运行时 | 本地编辑使用 GPUI，在线文档使用系统 WebView，无 Electron |

<sub>Apple M5 Pro / 48 GiB，macOS 26.5，Release 构建。测试方法和复现脚本见 [docs/PERFORMANCE.md](docs/PERFORMANCE.md)。</sub>

## 功能

- **编辑**：标题、有序 / 无序列表、待办、引用、代码块（语法高亮）、LaTeX 公式、表格（可合并单元格）、高亮块、分栏、图片、附件、封面。支持 `#`、`-`、`[]`、`>` 等 Markdown 快捷输入和 `/` 斜杠菜单。
- **整理**：文档树、置顶、收藏、全文搜索、回收站；文档目录与正文可分别滚动，点击目录可定位正文。搜索也包含已保存的飞书链接。悬停在侧栏文档、飞书链接或目录上，点击行尾「设置图标」，可使用同一个选择器搜索、设置或移除图标；文档图标也可通过右键或「…」菜单设置。
- **标签**：本地文档和在线页面共用顶部标签栏；再次打开同一项会切回已有标签，切换时保留编辑和网页状态。标签过多时可在标签栏滚动，关闭标签不会删除文档。
- **导入导出**：导入 Markdown 和飞书 DocxXML；导出 Markdown、PDF，也可以直接打印。从飞书或网页粘贴时保留结构。
- **飞书链接**：在侧栏自定义目录中保存飞书文档链接，选择在线打开或下载为本地副本，用原生编辑器编辑，并按需同步受支持的内容修改。
- **快捷键**：按 `⌘ /` 查看全部快捷键，每一个都能改。`Ctrl + +` / `Ctrl + -`（macOS 使用 `⌘`）调整正文显示比例，不影响侧栏或文档目录；标题栏显示当前比例，点击可恢复 100%。
- **主题**：内置 Craft、飞书、Notion、Obsidian 四套风格；侧栏与本地正文默认使用 Typora 参考截图的浅色纯色背景，已设置的自定义正文背景保持不变。点击文档顶栏「外观」，或在「设置 › 外观 › 文档默认外观」点击「设置」，可在弹窗中选择字体、文字颜色和页面背景色。背景支持输入 `#FFFFFF` 这样的六位十六进制颜色；「设置 › 外观」中的「侧栏外观」和「文档目录外观」分别设置各自的字体与字号。也可用 CSS 自己写主题，见 [docs/THEMES.md](docs/THEMES.md)。

## 飞书链接与手动同步

1. 在线打开在应用内嵌入飞书网页，无需安装 `lark-cli`；Windows 使用 WebView2 运行时，macOS 使用系统 WKWebView。首次使用可能需要在内嵌网页中登录飞书，能否查看或编辑由飞书账号权限决定。
2. 在侧栏「飞书链接」中点击「添加链接」，在弹窗里输入名称和 `/docx/` 或 `/wiki/` 链接，选择默认打开方式，并在目录列表中选择保存位置；较长的目录列表可以滚动。悬停链接或目录后点击行尾铅笔，或右键选择「重命名」，按 Enter 保存、Esc 取消；链接还可在此修改默认打开方式。在线编辑直接使用飞书网页，不创建本地副本，也不会自动更新已下载的副本。
3. 下载或同步前，安装官方 [lark-cli](https://github.com/larksuite/cli)（如 `npx @larksuite/cli@latest install`），运行 `lark-cli config init` 配置应用，再运行 `lark-cli auth login --domain docs --domain drive --domain wiki` 授权用户身份。账号需要对应文档的读取和编辑权限；`wiki` 权限用于知识库链接。选择「下载/打开本地副本」时，点击链接会打开已有副本；有多个副本时打开最近使用或修改的一个。没有副本时，选择「我的文档」中的目标目录后再下载；单篇文档最多同时下载 10 个素材，窗口底部显示进度。行尾按钮可随时在线打开或重新下载为新副本，重新下载不会覆盖已有笔记。
4. 编辑副本后点击标题栏「同步」，才会尝试写回该链接对应的飞书文档。

应用内在线页面使用网页自身的字体、文字颜色和背景，不受本地文档外观设置影响。侧栏和本地文档目录仍可分别调整字体及字号。

通过 `lark-cli` 的飞书下载、同步和网络图片使用直连，不使用系统或环境中的代理设置；如果当前网络必须通过代理访问，相关请求可能失败。应用内的在线网页使用系统 WebView 的网络设置。

同步支持文档标题、普通段落和列表的文字及样式、链接、待办状态，以及受支持块的新增、删除和移动；也支持图片、附件和表格的常见修改，包括单元格内容、行列增删、列宽及符合安全条件的合并或拆分。图片和附件替换时会上传素材并更新云端块身份。

为了不丢失云端内容，含评论或无法识别的内容、修改块类型、裁剪图片、修改单元格背景或垂直对齐、移动表格行列、合并非空单元格等无法安全处理的操作，会在写入前拒绝。合并表格还需要飞书返回的单元格身份和合并信息能够确定对应关系；未修改的复杂块保持不变。文档图标属于本地外观，不会同步到飞书，也不会阻止其他内容同步。

同步前会比较云端版本；版本已变化时停止写入并保留本地副本。版本检查并非原子锁，同步过程中仍应避免其他人同时编辑。同步中断可能已写入部分块；此时请重新下载并核对云端内容后再处理本地修改。

此功能需要从当前源码构建；上游官网下载的安装包不包含这些改动。

## 你的文件在哪

默认在 `~/Documents/Quiet.md`（Windows 是「文档」下的 `Quiet.md`）。每篇笔记是 `docs/` 里的一份 XML，图片和附件在 `assets/`。保存是原子的：先写临时文件再改名，崩溃也不会留下写了一半的笔记。

目录结构和格式细节见 [docs/FORMAT.md](docs/FORMAT.md)。

## 参与开发

quiet.md 还在 Beta，有很多可以一起做的事：修 bug、补功能、适配更多平台。欢迎提 issue 和 PR。

从源码构建、测试和打包见 [CONTRIBUTING.md](CONTRIBUTING.md)，代码结构见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

## 许可

代码使用 [MIT](LICENSE) 许可。第三方代码、字体和素材见 [THIRD_PARTY.md](THIRD_PARTY.md)。

以下内容不在 MIT 范围内，fork 时请替换：

- `assets/icons/` 里的 67 个 Nucleo 图标使用 Nucleo 商业授权，请勿单独提取再分发；
- 「quiet.md」这个名字和应用图标。
