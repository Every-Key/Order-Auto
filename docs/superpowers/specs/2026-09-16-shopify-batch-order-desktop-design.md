# Shopify 批量订单桌面工具设计规格

## 1. 目标

构建一个供个人使用的 macOS 与 Windows 桌面程序。用户可以保存并检索多个 Shopify 店铺配置，搜索店铺中的现有商品变体与客户，并使用相同信息批量创建多张独立订单。

本工具只用于用户有权管理的 Shopify 店铺。第一版不提供云端服务、团队账号、OAuth 安装流程、商品创建、订单编辑或订单取消功能。

## 2. 技术方案

- 桌面框架：Tauri 2。
- 前端：React、TypeScript。
- 本地后端：Rust。
- 本地数据库：SQLite。
- Shopify 接口：Admin GraphQL API `2026-07`。
- 目标平台：macOS、Windows。
- 运行方式：完全本地运行，不依赖自建服务器。

应用内部的数据流为：

`React UI → Tauri Command → Rust 业务服务 → Shopify Admin GraphQL API`

Rust 业务服务同时读写本地 SQLite，用于保存店铺配置、批次与逐单执行结果。

## 3. 权限与凭据

每个店铺配置包含：

- 用户自定义的店铺显示名称；
- Shopify 店铺域名，规范化为 `<shop>.myshopify.com`；
- Admin API Access Token；
- 创建时间、更新时间与最近一次连接测试结果。

Token 按用户明确要求直接存入本机 SQLite，不接入 macOS 钥匙串或 Windows 凭据管理器。UI 默认以掩码显示 Token，只在编辑时经用户操作显示。应用必须提示：能读取本地数据库文件的人可能获得 Token。

Token 至少需要以下权限：

- `read_products`：搜索商品与变体；
- `read_customers`：搜索现有客户；
- `read_orders`：恢复批次并核对结果不确定的创建请求；
- `write_orders`：创建订单。

`orderCreate` 仅支持使用 offline token 的应用。若 Shopify 对受保护客户数据有额外限制，连接测试必须单独报告客户读取权限不可用，不能笼统显示为 Token 无效。

## 4. 模块边界

### 4.1 StoreRepository

负责店铺配置的新增、检索、读取、编辑和删除。它只操作 SQLite，不直接调用 Shopify。

### 4.2 ShopifyClient

负责 GraphQL 请求、认证头、API 版本、请求超时、错误解析、限流等待和临时错误重试。上层服务不拼接 HTTP 请求。

### 4.3 CatalogService

按商品名称或 SKU 搜索现有商品，返回商品及具体变体的 ID、标题、SKU、价格和可用库存信息。创建订单前必须选择具体变体。

### 4.4 CustomerService

支持搜索 Shopify 现有客户，并把手动输入的邮箱、姓名、电话和地址转换为 `orderCreate` 接受的客户输入。客户信息允许留空。

### 4.5 OrderBatchService

验证订单模板和批量数量，创建批次及批次项，按顺序逐单调用 `orderCreate`，发布进度事件，并处理停止、恢复、失败重试和结果核对。

### 4.6 JobRepository

保存批次、逐单状态、Shopify 订单结果、尝试次数和错误信息。程序重启后，UI 可以重新加载历史批次。

## 5. 本地数据模型

### 5.1 stores

- `id`：本地 UUID；
- `display_name`：店铺显示名称；
- `shop_domain`：规范化后的店铺域名，唯一；
- `access_token`：Admin API Token；
- `last_connection_status`：最近测试结果；
- `last_connection_message`：最近测试详情；
- `created_at`、`updated_at`。

### 5.2 batch_jobs

- `id`：批次 UUID；
- `store_id`：目标店铺；
- `order_template_json`：批次开始时冻结的订单输入快照；
- `requested_count`：请求创建的订单数；
- `status`：`pending`、`running`、`stopping`、`completed`、`completed_with_errors`、`paused`；
- `created_at`、`started_at`、`finished_at`。

### 5.3 batch_items

- `id`：批次项 UUID；
- `batch_id`：所属批次；
- `sequence_number`：从 1 开始的序号；
- `source_identifier`：`orderpilot/<批次UUID>/<序号>`，全局唯一；
- `status`：`queued`、`creating`、`succeeded`、`failed`、`uncertain`、`stopped`；
- `attempt_count`：尝试次数；
- `shopify_order_id`、`shopify_order_name`；
- `error_code`、`error_message`；
- `created_at`、`updated_at`。

## 6. 店铺管理体验

店铺管理页提供：

- 按显示名称或域名实时检索；
- 新增、编辑和删除店铺配置；
- 输入显示名称、店铺子域名或完整 `myshopify.com` 域名、Admin API Token；
- 测试连接；
- 显示缺失的权限或无效凭据；
- 保存后在创建订单页直接切换店铺。

删除店铺配置必须二次确认。若该店铺存在历史批次，删除配置不删除批次与逐单结果，只移除凭据和可用店铺入口。

## 7. 创建订单体验

### 7.1 选择店铺与商品

用户先检索并选择已保存店铺。程序确认连接正常后，允许按商品名称或 SKU 搜索。搜索结果必须展开到具体变体；用户选择一个变体并设置每张订单中的商品数量。

### 7.2 客户信息

用户可以：

- 搜索并选择 Shopify 现有客户；
- 手动输入邮箱、姓名、电话和收货地址；
- 留空，创建无客户订单。

客户或地址字段只在用户填写时发送。应用不为填充界面而虚构客户资料。

### 7.3 付款状态

支持 `PENDING` 与 `PAID`，默认 `PENDING`。选择 `PAID` 后，最终确认步骤必须显示二次警告，说明该状态只适用于已经通过其他渠道完成收款的订单。

### 7.4 批量数量

批量创建数量默认值为 `1`，必须为 1 到 100 的整数。它表示要创建多少张独立 Shopify 订单，不等于每张订单中的商品数量。

确认页展示：店铺、商品变体、每单商品数量、客户信息、付款状态、每单金额和预计创建订单数。用户确认后，订单模板被冻结到 `batch_jobs.order_template_json`，运行期间不受界面后续编辑影响。

## 8. 批量执行

批次按序逐张创建订单，避免短时间并发请求触发 Shopify 限流。每张成功订单立即写入 SQLite，再处理下一张。

进度页展示：

- 完成数量与总数；
- 成功、失败、待确认和等待数量；
- 当前处理序号；
- 每张订单的状态、Shopify 订单号和错误原因；
- “停止未执行订单”；
- “仅重试失败项”；
- 导出结果。

停止操作只阻止尚未提交的批次项。已经创建的 Shopify 订单不会自动取消。程序重启后不会自动继续未完成批次，用户必须在历史记录中明确点击“继续”。

## 9. 防重复策略

每个批次项在第一次请求前生成并持久化唯一 `sourceIdentifier`。创建订单时将它写入 Shopify `OrderCreateOrderInput.sourceIdentifier`。

如果请求明确返回成功，保存 Shopify 订单 ID 与订单号。如果请求明确返回业务错误，保存失败原因。如果出现超时、断网或响应解析失败，不能确认 Shopify 是否已经处理请求时：

1. 将批次项标记为 `uncertain`；
2. 使用 Shopify `orders` 查询的 `source_identifier` 过滤器检索；
3. 在 30 秒内最多核对 3 次，以容纳订单搜索索引的短暂延迟；
4. 若找到订单，则补记为成功；
5. 若查询失败或多次查询仍未找到，保持 `uncertain`，允许用户再次核对，但不能自动重新提交；
6. 只有用户明确选择“强制重试”并确认可能造成重复订单的风险后，才重新执行该项。

相关 Shopify 官方能力：

- [`OrderCreateOrderInput.sourceIdentifier`](https://shopify.dev/docs/api/admin-graphql/latest/input-objects/OrderCreateOrderInput)
- [`orders` 的 `source_identifier` 过滤器](https://shopify.dev/docs/api/admin-graphql/latest/queries/orders)

## 10. 错误与重试

- Shopify 限流与临时服务错误：遵守返回的限流信息并延迟重试，单项最多自动重试 3 次；
- 商品失效、库存问题、地址错误等业务错误：当前项失败，继续后续项；
- Token 无效、店铺域名错误或缺少关键权限：暂停整个批次；
- 本地数据库写入失败：立即暂停，避免创建结果无法记录；
- 结果状态不确定：执行第 9 节的核对流程，自动核对不能确认时禁止自动重试；
- 操作员停止：把尚未开始的项标记为 `stopped`。

错误消息必须同时保留适合用户阅读的说明和用于诊断的原始 Shopify 错误代码，不在日志中输出完整 Token。

## 11. UI 页面

第一版包含四个主要工作面：

1. 店铺管理：检索、连接测试与配置维护；
2. 创建订单：商品变体、每单数量、客户、付款状态和批量数量；
3. 最终确认：完整摘要以及 `PAID` 二次警告；
4. 批次详情：进度、逐单结果、停止、恢复、失败重试与导出。

主布局采用已确认的“专注工作流”：左侧固定店铺列表，右侧显示当前任务，不在第一版增加统计仪表盘。

## 12. 测试策略

### 12.1 Rust 单元测试

- 店铺域名规范化；
- 批量数量验证；
- GraphQL 错误映射；
- 重试分类与退避；
- 批次状态转换；
- 唯一 `sourceIdentifier` 生成；
- 不确定结果的核对决策。

### 12.2 数据库集成测试

- 店铺配置 CRUD 与检索；
- 批次和批次项原子写入；
- 进度恢复；
- 删除店铺后保留历史批次；
- 同一 `source_identifier` 的唯一约束。

### 12.3 Shopify 客户端测试

使用模拟 HTTP 服务覆盖：成功、GraphQL `userErrors`、401/403、限流、服务端错误、超时、无效响应与恢复查询。测试日志不得包含 Token。

### 12.4 前端测试

- 店铺检索与配置表单；
- 商品与变体选择；
- 客户三种模式；
- `PENDING` 默认值与 `PAID` 二次确认；
- 批量数量边界；
- 进度事件与结果列表；
- 停止、继续和仅重试失败项。

### 12.5 手动验收

先连接 Shopify Development Store，选择一个现有商品变体，以相同信息批量创建 10 张测试订单。最终必须列出 10 个明确结果；模拟超时并恢复后，任何已经成功的订单都不能因重试被重复创建。

在 macOS 与 Windows 上分别验证安装、首次启动、本地数据库读写、店铺配置、批量创建和程序重启恢复。

## 13. 第一版验收标准

- 能保存、检索、编辑和删除多个本地店铺配置；
- 能验证 Shopify 连接与必要权限；
- 能搜索商品名称或 SKU 并选择具体变体；
- 能选择现有客户、手动填写客户或不关联客户；
- 能选择 `PENDING` 或 `PAID`；
- 能输入 1 到 100 的批量数量并创建对应数量的独立订单；
- 能实时展示并持久化每张订单的结果；
- 能停止未执行订单、恢复批次、仅重试明确失败的项目并导出结果；
- 对结果不确定的请求先核对；无法确认时禁止自动重试，并对强制重试进行风险确认；
- macOS 与 Windows 均可安装和运行。
