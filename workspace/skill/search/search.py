#!/usr/bin/env python3
"""搜索引擎搜索脚本：使用 ddgs 库 (DuckDuckGo)"""

import sys
from ddgs import DDGS

def search(keyword: str, max_results: int = 10):
    try:
        ddgs = DDGS()
        results = list(ddgs.text(keyword, max_results=max_results))
    except Exception as e:
        print(f"❌ 搜索失败: {e}")
        return

    if not results:
        print("⚠️ 未找到搜索结果")
        return

    for i, r in enumerate(results):
        print(f"--- [{i + 1}] ---")
        print(f"标题: {r['title']}")
        print(f"链接: {r['href']}")
        if r.get('body'):
            print(f"摘要: {r['body']}")
        print()


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("用法: python3 search.py <关键词> [结果数量]")
        sys.exit(1)

    keyword = sys.argv[1]
    max_results = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    search(keyword, max_results)
