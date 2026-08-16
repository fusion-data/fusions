# 语音协议客户端统一抽象（TTS providers）

> 基线：2026-08-15 三家供应商（MiniMax / 豆包 / 阿里百炼）接入定稿。本文记录
> `providers::speech` 底座 + 三家协议客户端的**代码无法表达的设计事实**：切分
> 判据、供应商协议差异实锤、计费语义与校准点。业务语义（emotion canonical、
> 降级标记、vendor 选型）在消费仓 creative-vendor，本文不描述。

## 1. 分层切分

| 层 | 归属 | 内容 |
|---|---|---|
| 协议客户端 | `fusion-ai::providers::{minimax, volcengine, dashscope}` | 请求构造、鉴权头、流式解析（SSE / JSON 行）、错误码分类、audio 编解码（hex/base64）、容器封装（WAV 头） |
| 公共底座 | `fusion-ai::providers::speech` | `SpeechError`（重试分级谓词）、`AudioPart`、`SseDataParser`、`pcm_wav_header`、`detect_audio_container`、hex 工具 |
| 业务适配 | 消费仓 | emotion canonical 转译、`DroppedCapability` 降级标记、音色代号命名策略（前缀）、Resource-Id 路由判据、计费规避策略（demo_audio 优先 vs 回落合成） |

**不定义统一 trait**：各家请求参数差异是本质的（MiniMax 枚举 emotion / 豆包
语气指令 + Resource-Id 路由 / 阿里无音频参数 + 600 字符分段），统一请求模型
会变上帝对象。三家客户端方法形态**鸭子对齐**（`clone_voice` / `synthesize` /
`synthesize_stream → BoxStream<AudioPart>`），消费仓各自薄适配。

## 2. 供应商对比 SSOT（协议 + 价格 + 能力，2026-08-15/16 实测 + 官方计费口径）

> 本表是三家语音供应商**比价的唯一真相源**（用户决策 2026-08-16 迁入；消费仓
> `media-pipeline-vendor-strategy.md` §2 与其他文档只引用不重复）。接入状态 /
> `*_dropped` 标记等为消费仓（hetu-creative）语境；价格均为官方定价页实锤，
> 汉字 ×2 字符口径（三家一致，单价可直接对齐比较）。

| 维度 | MiniMax T2A V2 | 豆包语音 V3（复刻 2.0 / 合成 2.0） | 阿里百炼（Qwen-TTS 系，hetu-creative 接入线） |
|---|---|---|---|
| 接入状态 | 已接入（合成/流式/字幕/发音词典全通） | 已接入后**停用**（2026-08-15 全链实测通过；2026-08-16 用户决策停用——配置层封 key、实现保留，恢复 = 取消 env 注释 + 重启，实现已验证无代码改动） | 已接入 · **消费仓系统默认**（2026-08-16 全链实测：enrollment ~6s → 试听 → 流式 WAV → 16 页 FULL 构建完成；同日用户决策定档默认） |
| 鉴权 | Bearer + GroupId query | `X-Api-Key` + `X-Api-Resource-Id` + `X-Api-Request-Id` | Bearer |
| 复刻形态 | files/upload + voice_clone 两步，voice_id 调用方自定义 | 单接口 base64，代号调用方生成，返回 `demo_audio`（训练产物，试听不触发首次合成计费） | `qwen-voice-enrollment` 单接口，base64 Data URL 或公网 URL，voice 名 vendor 生成 |
| 复刻样本形态 | bytes 上传（两步 multipart，仅消费首段） | bytes 上传（单接口 JSON base64，单段 ≤10MB） | Qwen-TTS 支持 base64 data URI（已接入）；CosyVoice 只收**音频 URL**（消费仓 fs storage presign 不可公网达——形态不可行） |
| 复刻价格 | 复刻免费，**首次合成收 9.9 元/音色解锁费**（试听合成另按所选模型标准价计费） | 训练免费；**音色槽位按音色个数计费**（预付费/后付费均可，金额以控制台定价为准，试用有赠送额度）；复刻音色合成走「声音复刻模型 2.0」商品按字符计费 | Qwen-TTS 系 0.01 元/个（开通 90 天内 1000 次免费，失败不计费）；CosyVoice 系免费但形态不可行（见上行） |
| 复刻可用性 | **真实创建被账户 2038 挡**（付费套餐/实名，消费仓 T-012 未解除）；素材另有时长限制（<10s 报 2037） | 可用；后付费必须开通**音色槽位**资源（`volc.megatts.timbre`，漏开 403）；预付费槽位 15 次训练；试听音色 7 天未正式调用删除 | 可用；音色上限 1000 个/账号（与 CosyVoice 系配额独立，达限创建报错需手动删除释放）、1 年未用自动清理；**绑定 target_model 不可跨模型**（换合成模型 = 全量重克隆） |
| 克隆后免费试听 | 无（克隆后自行合成计费） | **训练自带 demo_audio**（1h 有效，落消费仓 storage 长期可用） | 无自带试听，回落正式合成——**无「首次合成转正费」**，预览按字符计费（几十字符，费用可忽略） |
| 合成价格 | **speech-2.8-turbo（消费仓默认，2026-08-16 切换）2.0 元/万字符**；hd 线 3.5（老默认 speech-02-hd 同价）；turbo/hd 参数面同构——字幕直返/发音词典/情感实测可用，env `MINIMAX_TTS_MODEL` 可覆盖 | 资源包 2.2–2.8 元/万字符（有效期 1 年，过期作废）；后付费小时结、超额累进按天 | **qwen3-tts-vc / qwen3-tts-flash 均 0.8 元/万字符**（三家最低）；CosyVoice v3-plus 2.0 / v3.5-plus 1.5 / flash 线 0.8–1.0 |
| 免费试用额度 | **无**（官方定价页语音类无免费档） | **合成 2.0 与复刻 2.0 各 20000 字符（半年）**；预付费槽位赠每音色 15 次训练 + 试听字符 | **每模型合成 1 万字符（开通 90 天内）**；复刻 90 天内 1000 次免费（失败不占次数、删除不恢复次数） |
| 情感 | 枚举参数（happy/sad/calm + 2024 细分值，消费仓转译层全消化） | 2.0 无枚举：预置音色走 `context_texts` 语气指令；**复刻音色不支持指令**（`emotion_dropped`） | **无情感参数**（Instruct 系独占且克隆音色不支持指令；`emotion_dropped`） |
| 语速/音量 | speed/vol 参数 | speech_rate/loudness_rate 参数（倍率 1.0 基准内部映射 [-50,100]） | **无音频参数**（`speed_dropped`，非默认值才标） |
| 字幕 | **vendor 直返 SRT**（`subtitle_enable`，hex 或 OSS URL 两形态，三家唯一） | 字级时间戳存在（`enable_subtitle`，仅 2.0 中英文）但消费仓未接（`subtitle_dropped`） | 无 |
| 发音词典 | **tone/replace 两模式**（`pronunciation_dict`，三家唯一） | 无（`pronunciation_dropped`） | 无 |
| 流式协议 | SSE，`data.audio` **hex**，末块 `extra_info` 判终 | HTTP chunked **JSON 行**，`code=20000000` 结束行 | SSE（`X-DashScope-SSE`；`id:`/`event:result` 前缀行），`audio.data` **base64**——首块自带 44 字节 WAV 头（vendor 侧 size 用 0x7FFFFFFF 流式占位）、次块起裸 PCM，末块 data 空 + url 非空（2026-08-15 联调实锤，规格 24k/16bit/mono） |
| 单请求文本上限 | 10000 字符 | 2048 字符 | **600 字符**（fusion-ai 客户端自动分段：句末标点 → 逗号 → 硬切；分段不改计费） |
| 业务错误 | HTTP 200 + `base_resp.status_code`（1002 限流；1008/1004/2013 永久） | 8 位码：4 开头永久 / 5 开头瞬态 / 45000000+quota/concurrency 限流 | code 串：`Throttling.*` 限流 / `InternalError` 等瞬态 / **未知码按永久**（保守不盲重试） |
| 整段合成音频 | hex → mp3/pcm/wav | base64 → mp3/pcm/wav | PCM 直拼 + **客户端封装 WAV 头**（唯一出 WAV 的链路；收集后回写准确 size） |
| 语种/方言 | 中英日韩等多语种 | 多语种 + 方言（北京/东北/粤语等）+ SSML + LaTeX 朗读（教育场景） | 多语种 |

**读表结论**：**阿里接入线是三家最低合成单价**（0.8 元/万字符，约为豆包资源包的 1/3、MiniMax turbo 的 2/5），且复刻近乎免费、无解锁费——对「每位讲师一个音色」的规模场景成本结构最优；**能力独占项各有归属**：MiniMax 独占字幕直返 + 发音词典但克隆被账户权限挡；豆包复刻全通且自带免费试听、预置音色支持语气指令情感；阿里链路无情感/语速/字幕/发音词典（消费仓全维度 `*_dropped` 可追溯），单价优势换能力面。豆包计费商品与实现对应：预置音色合成 = 「语音合成模型 2.0」（`seed-tts-2.0`）、复刻训练与合成 = 「声音复刻模型 2.0」（`seed-icl-2.0`）、克隆前置 = 「音色槽位」计费项（开通清单见消费仓 `docs/uat/assets.md` 场景七-补-2）。

## 3. 关键决策（代码不能表示的事实）

- **阿里走 Qwen-TTS 系而非 CosyVoice 系复刻**：CosyVoice 系（`voice-enrollment`）
  只接受**公网 URL**（vendor 侧拉取样本），消费仓 fs storage 的 presign URL 不可
  公网达——形态不可行。Qwen-TTS 系（`qwen-voice-enrollment`）支持 base64 Data
  URL，对齐样本 bytes 形态。代价：克隆音色无指令控制（emotion 恒 dropped）。
- **target_model 绑定**：阿里克隆音色合成时 model MUST 与复刻 target_model 完全
  一致（官方约束）。enrollment 产物 voice 命名实锤（2026-08-15 联调）：
  `qwen-tts-vc-{preferred_name}-voice-{timestamp}-{rand}`——**无数字 3**，与
  target_model `qwen3-tts-vc-...` 不同串，消费仓路由前缀以此为准。
- **阿里整段合成统一走 SSE 而非非流式 url**：多段（>600 字符）时中间块直拼
  天然安全（首块自带 WAV 头、次块裸 PCM，段间直拼即合法流），非流式 url 是
  完整 WAV 文件、多段需剥头解析。单条网络路径减少联调校准面。收集完成后
  回写准确 RIFF/data size（vendor 占位 0x7FFFFFFF）。
- **流式 WAV 头 `data_len=0xFFFFFFFF` 占位**：流式场景总长未知，浏览器按 EOF
  收尾容忍该值（消费仓前端把 chunks 拼一个 Blob 播放）。
- **JSON 行流结束后处理残行**：最后一行无结尾换行时（错误行 / Done 行常是 body
  末行），行缓冲 MUST flush 残行——否则错误被静默吞掉、流以空音频「正常」结束。
  验证锚点：`volcengine::speech::tests::stream_error_lines_and_http_errors_classified`。
- **QwenTts 旧 `QwenTtsError` / `QwenTtsAudio` 移除**：spike 期产物，消费仓零引用
  （grep 实锤），统一迁移 `SpeechError`；`synthesize` 返回 WAV `Bytes`。
- **subtitle URL 下载语义在消费仓**：客户端提供 `download_subtitle`（no_proxy
  client，规避 OSS 经代理 SSL 握手失败），「下载失败降级无字幕不阻塞音频」是
  业务决策，不在协议层。

## 4. 计费语义（接入侧必须知道的坑）

> 本节只记**单家接入坑**（试听通道选择的依据）；价格数字与比价见 §2。

- **豆包**：后付费音色训练免槽位费，**首次正式合成即「转正」收槽位费**——试听
  MUST 走训练响应 `demo_audio`（协议层已解码返回），MUST NOT 回落正式合成。
- **MiniMax**：复刻免费但首次用该音色合成收解锁费（无自带试听，回落合成
  即计费——消费仓已知语义）。
- **阿里**：复刻按个计费（有免费额度，失败不计费），**无转正费**
  ——试听回落正式合成仅按字符计费（预览文案几十字符，费用可忽略）。

## 5. 验证锚点

- 底座：`providers/speech/mod.rs` tests（错误分级 / SSE 分帧跨 chunk / WAV 头
  字段 / 容器探测 / hex 往返）
- MiniMax：`providers/minimax/tts.rs` tests（hex 解码 / subtitle 两形态 / 业务码
  分类 / SSE 流 / 复刻两步）
- 豆包：`providers/volcengine/speech.rs` tests（JSON 行流 is_last / ICL 路由 +
  model / 流内错误分类 / demo_audio 解码降级 / Debug 脱敏）
- 阿里：`providers/dashscope/{qwen_tts, voice_enrollment}.rs` tests（分段切分 /
  SSE chunk 形态 / Data URL magic 探测 / preferred_name 校验 / 错误码分类）
