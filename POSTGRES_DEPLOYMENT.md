# PostgreSQL 部署

将 `CACHE_BACKEND=postgres` 和 `IMAGE_CACHE_BACKEND=postgres` 配置为 PostgreSQL 缓存；SQLite、Redis 和 S3 仍是可选后端。

通过部署环境设置 `DATABASE_URL`，不要把实际凭据提交到仓库。为 unfurl 分配专用数据库或 schema 以及最小权限登录角色；使用共享数据库时，在启动前配置该角色的 `search_path`，避免跨服务表名冲突。

首次切换后端应使用新的、隔离的数据库或 schema，并在启动前验证导入数据的条数和内容。保留原数据与可恢复备份；不要把 PostgreSQL 后端指向 SQLite URL。

测试通过 `TEST_DATABASE_URL` 连接一次性的本地 PostgreSQL 实例。测试会创建和删除隔离 schema，绝不能使用生产数据库或生产凭据。

缓存逻辑预算、过期宽限及存储空间限制见 [服务说明](server/README.md)。
