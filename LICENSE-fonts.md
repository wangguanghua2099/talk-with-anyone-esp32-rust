# Font data notices / 字库数据版权声明

本仓库的**代码**按 [MIT](LICENSE) 授权。

`src/fonts/efont_cn_14.bin` 是**衍生数据**，不是本仓库的原创作品：它由
[LovyanGFX](https://github.com/lovyan03/LovyanGFX) 的
`src/lgfx/Fonts/efont/lgfx_efont_cn.c` 中 `lgfx_efont_cn_14[]` 数组（u8g2 点阵格式）
提取成纯二进制 blob。因此它同时带有两层上游声明：

| 层 | 权利人的声明 | 许可 |
|----|--------------|------|
| 字形数据本身 | `(c) Copyright 2000-2001 /efont/ The Electronic Font Open Laboratory` | BSD-3-Clause 风格的宽松许可，见 [`licenses/efont-COPYRIGHT.txt`](licenses/efont-COPYRIGHT.txt) |
| u8g2 点阵编码与转换 | `Copyright (c) 2020 lovyan03` | FreeBSD License（BSD-2-Clause 风格），见 [`licenses/LovyanGFX-FreeBSD.txt`](licenses/LovyanGFX-FreeBSD.txt) |

## 允许什么

两份许可都明确允许 **source 与 binary 形式的再分发和使用，允许修改、允许商用**。
把它们编进固件、开源发布、商用产品里都没有问题。

## 需要遵守什么（本仓库已按此处理）

1. **源码形式再分发**：保留上游版权声明、条件列表与免责声明。
2. **二进制形式再分发**：须在随附的文档或其他材料中复现上述声明 —— 所以本仓库把两份
   原文放进了 `licenses/`，并在 README 的 License 一节指出这一点。打成 crate 发布时，
   用 `include` / `package` 配置把 `licenses/` 一并带上即可满足。
3. **不得背书**：不得使用 /efont 团队或其贡献者的名义为衍生产品做宣传或背书。

## 一句话结论

代码可以按 MIT 发布；字库 blob **不需要**改成 MIT，也**不能**宣称是本项目的 MIT 作品 ——
保留上游声明随包分发即为合规。若你只需要一个不带中文点阵的纯工具链（转换器 + 运行时），
把 blob 换成"由使用者自备上游字体源"的模式，就完全不涉及数据分发问题。
