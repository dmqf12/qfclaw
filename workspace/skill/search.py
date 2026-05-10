#!/usr/bin/env python3
"""搜索引擎搜索脚本：使用 requests 抓取 DuckDuckGo HTML 版搜索结果"""

import sys, urllib.parse, urllib.request, re, html

SEARCH_URL = "https://html.duckduckgo.com/html/?q={}"

def search(keyword: str, max_results: int = 10):
    url = SEARCH_URL.format(urllib.parse.quote(keyword))
    req = urllib.request.Request(url, headers={
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
    })
    
    try:
        resp = urllib.request.urlopen(req, timeout=15)
        body = resp.read().decode("utf-8", errors="ignore")
    except Exception as e:
        print(f"❌ 请求失败: {e}")
        return

    # 解析结果
    # DuckDuckGo HTML 版结构: <a class="result__a" href="...">标题</a> <a class="result__snippet" ...>
    results = re.findall(
        r'<a\s+rel="nofollow"\s+class="result__a"\s+href="([^"]+)"[^>]*>(.*?)</a>',
        body, re.DOTALL
    )
    
    snippets = re.findall(
        r'<a\s+class="result__snippet"[^>]*>(.*?)</a>',
        body, re.DOTALL
    )

    count = 0
    for i, (link, title_raw) in enumerate(results):
        if count >= max_results:
            break
        
        title = html.unescape(re.sub(r'<[^>]+>', '', title_raw)).strip()
        if not title:
            continue
        
        snippet = ""
        if i < len(snippets):
            snippet = html.unescape(re.sub(r'<[^>]+>', '', snippets[i])).strip()
        
        print(f"--- [{count + 1}] ---")
        print(f"标题: {title}")
        if link.startswith("//"): link = "https:" + link
        parsed = urllib.parse.urlparse(link)
        q = urllib.parse.parse_qs(parsed.query)
        real_link = q.get("uddg", [link])[0]
        print(f"链接: {real_link}")
        if snippet:
            print(f"摘要: {snippet}")
        print()
        count += 1

    if count == 0:
        print("⚠️ 未找到搜索结果")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("用法: python3 search.py <关键词> [结果数量]")
        sys.exit(1)

    keyword = sys.argv[1]
    max_results = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    search(keyword, max_results)
