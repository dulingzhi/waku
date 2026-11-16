# 待真机验证清单

本清单列出必须在真实环境点一遍的项。这些功能都有单测，其中加密层和 relay 还有改坏验证和跨语言向量——**但没有任何一条在真实设备上跑过**。

一到四章大体按「坏了有多难查」排序。第五章是身份密钥，第六章是 relay 配对，按**操作顺序**排，因为它是一条连贯的链路：环境没搭起来，后面的项就无从谈起。

第六章里 6.4 的 SAS 比对是整份清单最不能省的一条，原因写在那里。

## 一、桌面端

### 1. 远程 daemon 下添加项目

启动一个远程 daemon（另一台机器或本机另开端口），桌面端连过去，点「添加项目」。

- [ ] 能打开目录浏览对话框（而不是弹「该功能尚不可用」的 toast）
- [ ] 列出的是 **daemon 主机**的目录，不是本机的
- [ ] 能进入子目录、返回上级、跳主页
- [ ] 过滤框能筛选目录
- [ ] 选中后能创建项目并进入会话

**坏了会怎样**：如果是本机目录，说明 `is_remote()` 判断失效，比不工作更糟。

### 2. 远程 daemon 下开终端

同一个远程连接，右面板 Terminal 标签。

- [ ] 面板能打开（不再被禁用）
- [ ] 能执行 `ls`、`cd`、`clear`
- [ ] **输出是连续完整的**，没有丢字或断裂
- [ ] 拖拽窗口边缘改变大小，终端跟随 resize
- [ ] 关闭标签页后，daemon 侧进程真的退出（可在 daemon 机器上 `ps` 确认）

**风险点**：订阅在 `openTerminal` 成功之后才建立，理论上有个极窄的窗口会丢输出。如果发现**开头几个字符缺失**，就是这个竞态。

**背压未处理**：高频输出（比如 `yes` 或 `cat` 大文件）可能让内存涨。观察到卡顿或内存飙升就是这个原因，不是 bug，是已知缺口。

### 3. 本地终端没被弄坏

**回归检查**：不连远程 daemon，用本地 daemon 开终端。

- [ ] 一切和原来一样（这条最容易漏，因为重构动了 `TerminalTransport`）

### 3b. Windows daemon 终端（ConPTY）

只能在一台真的 Windows 机器上跑 `waku-daemon` 并连上终端。这次改动**没有一行经过运行验证**，全部是读上游源码后的推断。

- [ ] `openTerminal` 不报错，shell 真的起来了（不是卡在「启动中」）
- [ ] 提示符能出现，敲 `dir`、`cd`、`cls` 有正确输出
- [ ] **输出不是「每次 4ms 一跳」的卡顿观感**——`UnblockedReader` 无数据时返回 `Ok(0)` 而非阻塞，读循环靠 4ms 让出，这是最可能被感知为卡的地方
- [ ] 敲 `exit` 后客户端显示「进程已退出」，不是永远停在启动中
- [ ] **关掉终端标签后，`powershell.exe` 仍在任务管理器里**——这是设计取舍（不主动杀进程），不是 bug。数一下数量是否只增不减
- [ ] 然后退出 daemon，确认那些残留进程一并消失，且 daemon 退出本身不卡死（`Conpty::drop` 的 `ClosePseudoConsole` 会等 conout 抽干）
- [ ] 拖拽面板宽度，远端 shell 的换行跟随 resize
- [ ] 同时开三个终端标签，事件不串
- [ ] 中文输出/中文路径不乱码（ConPTY 代码页）

**坏了最可能是哪里**：读取循环。`Ok(0)` 在这个后端不等于 EOF，如果看到「刚起来就报已退出」，就是这里被当成了 EOF。

## 二、mobile 端

前置：起一个 daemon，mobile 连上。

### 4. fork

在一条已完成的助手回复下方找 fork 按钮。

- [ ] 按钮出现在**最后一块** markdown 下方，不是每块都有
- [ ] 点了会跳到新任务
- [ ] 派生期间按钮显示 spinner 且不可点
- [ ] **派生后返回能回到原任务**（用的是 `router.push`）
- [ ] 连 fork 两次，返回栈有两层，不是卡住

### 5. rewind

在用户消息气泡下方找 rewind 按钮。

- [ ] 只在**开启该轮的那条消息**上有，steer 消息上没有
- [ ] 点了弹 Alert（其他两端是编辑气泡，mobile 暂无）
- [ ] Alert 正文读起来别扭（借用了成功文案），**这是已知缺陷，不是 bug**
- [ ] 确认后真的回退了
- [ ] **回退后按钮状态立刻更新**（不等于 5 秒后才变——这是 turn refs 失效的验证点）
- [ ] 回退后不能立刻再点（否则会重复执行破坏性操作）

### 6. Skills

从 drawer 底部的技能按钮进入。

**列表**
- [ ] 按项目分组，分组标题显示项目名和数量
- [ ] 搜索框能匹配名称、描述、项目名，**以及安装路径**
- [ ] 禁用态的行有明显的视觉区分（名称变淡、徽章、图标变灰）
- [ ] 空态分三种：扫描中、加载失败（带重试）、无技能

**详情**
- [ ] 点行进入详情页
- [ ] **长 SKILL.md 滚动流畅**（一次性渲染所有 block，没有虚拟化；几百行应该没问题，超大 skill 要确认）
- [ ] **表格能在窄屏横向滚动**，且不与外层纵向滚动打架（手势冲突风险点）
- [ ] 大代码块不溢出屏幕
- [ ] 元信息：install 路径最多 3 条，超出显示 `+N more`
- [ ] 头部来源行正确显示 `Shared · Codex · in <项目>`

**启用开关**
- [ ] 切换后立刻生效（乐观更新），不闪回
- [ ] **切换后徽章和来源行状态一致**（不能出现「徽章说禁用、来源行说启用」）
- [ ] 切换期间开关变成 spinner，不是禁用形态
- [ ] **切换后返回列表，滚动位置正确**（这是刚修的退化点）

**删除**
- [ ] 第一次点击变成「确认删除」，不直接删
- [ ] 旁边有取消，点了能撤回
- [ ] **点别处失焦也会撤回**（onBlur disarm）
- [ ] 第二次点击才真删
- [ ] 删除后回到列表，**列表不闪已删项**
- [ ] 多个安装位置时，按钮旁显示位置数量，且该文字随语言切换

**交互**
- [ ] `Switch` 在标题行不挤（长 skill 名会 wrap，确认 Switch 不被挤走）
- [ ] 在 `ScrollView` 里操作 Switch 不会误触滚动
- [ ] 两段式按钮不会被误连续点击两次

## 三、附件上限

### 附件上限的行为是拒绝，不是截断

`attachment_upload_from_path`（`src/app/composer.rs:3529`）在超限时整份拒绝：

- 单个文件 > 32 MB → 整个拖拽失败
- 拖入的目录累计 > 32 MB 或 > 4096 个文件 → 整个目录失败

也就是说拖一个含 5000 个文件的目录进来，得到的是「超过上限」的拒绝，**而不是「只传前 4096 个」**。这是有意的（避免半上传的附件），但值得在真机上确认一次错误提示是否看得懂。

- [ ] 拖一个超大文件，错误提示能说清是大小超限
- [ ] 拖一个超多文件的目录，错误提示能说清是数量超限

## 四、三端通用的观感项

- [ ] 深色模式下所有新增 UI 的对比度
- [ ] 小屏（iPhone SE 375pt）上的可点性
- [ ] 震动反馈次数（已改成每个操作一次，确认没有多余）
- [ ] Alert 与 native header menu 的层级遮挡
- [ ] 配对码、SAS 数字在深色模式下的可读性（新增的 UI 里最依赖对比度的两处）

## 五、daemon 身份密钥（relay 加密的基础）

`crates/waku-core/src/daemon_identity.rs` 现在有调用者了：daemon 启动时加载它、把公钥放进 `DaemonReady`，并用它签握手 transcript。下面几项仍然需要真机确认。

### 5.1 Windows 上 `identity.key` 的访问控制

密钥落在 `~/.waku/identity.key`。unix 上文件用 `open(2)` 带着 `mode(0o600)` 创建（`daemon_identity.rs:228`），所以 inode 从诞生那一刻就不可被其他人读——不存在先写后改的窗口。

**Windows 上没有对应动作。** unix 的 `mode` 调用被 `#[cfg(unix)]` 排除，文件按 `~/.waku/` 继承的 profile ACL 创建。理由是「profile 目录默认只允许本人」，但**这个理由没有验证过**，需要实测。

用管理员 PowerShell 查实际 ACL：

```powershell
# 1. 先让 daemon 生成一次密钥（或手工建一个占位文件）
Get-Acl "$env:USERPROFILE\.waku" | Format-List

# 2. 看关键项：是否只有自己 + SYSTEM + Administrators
(Get-Acl "$env:USERPROFILE\.waku").Access |
  Select-Object IdentityReference, FileSystemRights, AccessControlType |
  Format-Table -AutoSize

# 3. 文件的 ACL（可能与目录不同）
(Get-Acl "$env:USERPROFILE\.waku\identity.key").Access |
  Select-Object IdentityReference, FileSystemRights |
  Format-Table -AutoSize
```

判断标准：

- [ ] 目录 ACL 里**没有** `BUILTIN\Users`（或 `Authenticated Users`）的读取项
- [ ] 文件 ACL 同样只含 `用户名`、`SYSTEM`、`Administrators`
- [ ] 用**另一个非管理员账户**尝试 `Get-Content $env:USERPROFILE\.waku\identity.key`，应当被拒绝

如果第三项能读到内容，说明继承 ACL 比预期宽松，**必须改成显式设 ACL**（需要给 `windows-sys` 加 `Win32_Security` 特性并调用 `SetNamedSecurityInfo`）。

### 5.2 密钥不被静默替换

- [ ] 手工把 `identity.key` 改成一个无效的 JSON，重启 daemon，确认**报错而不是自动生成新密钥**
- [ ] 确认原文件仍在原处（没有被删除或重命名）
- [ ] 恢复正确内容后能正常启动

**为什么重要**：静默重新生成会让所有已配对客户端验签失败，而且用户不会知道原因。

### 5.3 unix 权限位

CI 上 `the_identity_file_is_owner_only` 这条测试会跑：

- [ ] macOS / Ubuntu CI 上该测试通过（`0600`）
- [ ] 本地 Windows 上该测试被 `#[cfg(unix)]` 跳过（预期行为，不是缺失）

**记录方式**

每条打勾或记录现象。出问题时报给我：**哪个功能、什么现象、能否复现**。

远程终端那块尤其重要——它的订阅时序和背压都是设计上已知但未实测的风险，真实环境的行为可能和预期不同。

## 六、relay 配对（端到端加密的完整链路）

这一章的全部内容都只有单测和一次手动冒烟验证。**SAS 那条是全清单里最不能省的一项**：它是唯一能发现「relay 分别和两端各握手一次」的手段，而那种攻击下其他所有检查都会通过。

### 6.1 搭一个能跑的环境

三样东西，都可以在本机：

```powershell
# 1. relay（记下它打印的 address）
cargo run -p waku-relay -- --bind 127.0.0.1:45123

# 2. 让 daemon 去拨这个 relay
#    ~/.waku/settings.json，注意必须是纯 host:port
{ "relay_url": "127.0.0.1:45123", "relay_keep_listening": true }

# 3. 起 daemon，记下它输出的 pairing
cargo run -p waku-daemon -- --bind 127.0.0.1:45124
```

daemon 的 stdout 第一行现在是 `DaemonReady`，里面的 `pairing.code` 就是要用的配对码。同时 `pairing.one_time_notice` 说明它会在重启后失效。

- [ ] relay 启动并打印 `{"address":"...","role":"relay"}`
- [ ] daemon 日志出现 `waku-daemon relay: dialing 127.0.0.1:45123`
- [ ] daemon 日志出现 `waku-daemon relay: connected`
- [ ] `DaemonReady` 的 JSON 里有 `pairing` 块，`code` 形如 `waku://127.0.0.1:45123/<32位hex>#<16位hex>`

**如果第 2 步填了带 `ws://` 或带路径的地址**，daemon 应当启动即退出并报 `relay_url must be host:port with no scheme and no path`。

- [ ] 错误配置会让 daemon 启动失败，而不是生成一个坏掉的码

### 6.2 desktop 显示配对码

设置 → Daemon。这一页只有在 daemon 真的拨了 relay 时才出现配对码区块。

- [ ] 区块出现，显示完整的 `waku://...` 码
- [ ] relay 地址以徽章形式显示在旁边
- [ ] 「一次有效」的提示可见
- [ ] 点复制按钮，剪贴板里是**完整**的码（含 `#` 后面的指纹）
- [ ] 复制后按钮反馈变成「已复制」
- [ ] **把 `relay_url` 从 settings 删掉并重启 daemon**，这个区块整个消失，不留空白框

**如果删掉 relay 后区块还在**，说明 `when_some` 没有生效，界面在展示一个不存在的码。

### 6.3 web 用配对码连接

未连接时的 `ConnectionPanel`，切到配对码 tab。

- [ ] 输入框能粘进码，粘贴后立刻显示推导出的 scheme（本机应是「未加密（本机）」）
- [ ] 显示完整的目标 URL，形如 `ws://127.0.0.1:45123/<session>/client`
- [ ] token 字段仍然在，并有「令牌和配对码是两件事」的说明
- [ ] 填入正确的 token 后能连上
- [ ] 乱填的码 → 报「这不是配对码：…」，**不发起连接**

**改一个字符再试**（把 `#` 后面指纹的最后一位改掉）：

- [ ] 报「这个 daemon 不是它声称的那个」，并且**连接被断开**
- [ ] 报错文案说的是冒充，不是「fingerprint mismatch」

**这一项如果连上了**，说明指纹校验没有生效，是最严重的问题。

### 6.4 SAS 必须比对

连接成功后立刻弹层。

- [ ] 弹层出现，显示**十二位数字**，每四位一组
- [ ] 弹层盖住整个应用，背景不可点
- [ ] **只有两个按钮**：「我已核对，一致」「不一致，断开连接」
- [ ] 没有关闭按钮、没有 X、点背景不关、按 Esc 不关
- [ ] 点「不一致」→ 连接断开，回到连接页

**然后做真正的那一步**：在 daemon 那边看同一时刻的日志。

- [ ] daemon 日志有 `waku-daemon pairing code: XXXX XXXX XXXX`
- [ ] **这个值和你屏幕上的十二位数字逐位相同**

**这一条不等价于「能连上」。** 一个中间人 relay 可以让你连上、让指纹校验通过（它转发的是真 daemon 的签名），但会让两端显示**不同的**数字。如果两边的数字不一样，说明有人在中间转发，**立刻停用并报告**。

### 6.5 mobile 用配对码连接

mobile 的入口是 add daemon 的 sheet，形态和 web 不同。

- [ ] `DaemonEditor` 里有配对码字段
- [ ] 输入有效码后，地址字段自动填入推导出的 URL 并置灰
- [ ] profile 保存后重开应用，relay 和 fingerprint 还在，**session id 不在**
- [ ] 连接成功弹 SAS，`Modal` 盖住原生 header 和抽屉
- [ ] **Android 物理返回键按下去，弹层不关**
- [ ] 「不一致」→ 断开

**如果 Android 返回键能关掉弹层**，说明 `onRequestClose` 的空实现被系统当成「未处理」了，需要改成 `disabled` 或加 `BackHandler` 拦截。

### 6.6 一次性是真的

- [ ] 重启 daemon，旧的配对码**连不上**，新码能连
- [ ] 界面上有「配对码在 daemon 重启后失效」的说明，用户不会以为是 bug

### 6.7 证书路径（只有真远程才走）

前面都用 `ws://127.0.0.1`，那是明文但不出本机。真正跨网络时应当是 `wss://`。

- [ ] 把 `relay_url` 改成一个**非回环**地址（比如同一局域网的另一台机器），确认 web 端显示的 scheme 变成「已加密」
- [ ] 若 relay 没有有效证书，连接应当**失败**，而不是退回明文

**如果它退回 `ws://` 连上了**，说明 scheme 推导错了，这是安全缺陷。

### 6.8 三端一致

- [ ] web 和 mobile 同时连同一个 daemon，两边显示的 SAS **相同**
- [ ] 断开其中一个再连，那个连接显示**新的** SAS（每次握手都不同）

**记录方式**

每项打勾或记录现象。relay 这一章出问题时报给我：**哪一步、哪个端、看到了什么**。SAS 不一致是最优先的，无论其他项是否通过。

### 6.9 出问题时的排查顺序

先看 daemon 的日志，它比其他任何一端的错误信息都具体：

| 日志 | 含义 | 常见原因 |
| --- | --- | --- |
| 没有 `relay: dialing` | 没读到 relay 配置 | `relay_url` 拼错，或 daemon 读的是另一个 `~/.waku` |
| `relay: dialing` 后没有 `connected` | relay 不可达或拒绝 | relay 没启动；地址里的端口不对 |
| 反复 `waiting N ms before retry` | 拨号失败在退避 | 看这一行上面的具体错误；间隔递增是正常的 |
| `could not serve the connection` | 握手或读记录失败 | 客户端版本与 daemon 不一致（`PROTOCOL_VERSION` 差一级就会这样） |
| `refused a plaintext client` | 有旧客户端在连 | 那个客户端需要升级，不是 relay 的问题 |

**SAS 两边不一样时**，不要先怀疑代码：先确认两边连的是**同一个 session**。两个 daemon 各自发布了一个码、你连错了一个，症状也是「指纹不匹配」而不是「SAS 不同」。只有**指纹通过但 SAS 不同**才是中间人。

**连不上但没有任何错误**，先确认 daemon 的 `pairing.code` 和粘贴的码逐字符相同——包括 `#` 后面的 16 位。少一位会报解析错误，但多粘贴了空格不会。



