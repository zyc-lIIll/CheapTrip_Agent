# 游玩地点知识库

本目录下有两类知识库，供 LLM 在旅行规划时参考，并在攻略做好后自我迭代更新。

## 1. 景点地址知识库（JSON）

路径：`locations/{省份}.json`

记录各景点准确的 geocode 搜索名和坐标，供下次直接使用，跳过 geocode 调用。

```json
{
  "province": "北京市",
  "cities": {
    "北京": {
      "故宫": {
        "search_name": "北京故宫博物院",
        "city_hint": "北京",
        "lon": 116.404,
        "lat": 39.915,
        "note": "直接搜「故宫」可能返回台湾故宫，务必带 city"
      }
    }
  }
}
```

## 2. 旅游攻略知识库（Markdown）

路径：`guides/{省份}.md`

记录各城市的游玩攻略要点（预约/避坑/时长/注意事项），仅供参考参考。

## 目录结构

```
skills/knowledge/
  README.md              # 本文件
  locations/             # JSON 地址库（按省份）
    北京市.json
    浙江省.json
    ...
  guides/                # Markdown 攻略库（按省份）
    北京市.md
    浙江省.md
    ...
```

## 当前状态

尚未初始化具体省份文件。首次使用时 LLM 按默认策略搜索，攻略做好后逐步补充。
