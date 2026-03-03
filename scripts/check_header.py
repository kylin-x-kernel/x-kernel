import os

EXPECTED_HEADER = """// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details."""

def check_rs_file_headers(root_dir="."):
    """
    检查指定目录下的所有 .rs 文件是否包含预期的文件头。
    返回一个缺失文件头的文件路径列表。
    """
    missing_headers_list = []
    
    for root, dirs, files in os.walk(root_dir):
        # 忽略 target 目录以加快搜索速度
        if 'target' in dirs:
            dirs.remove('target')
            
        for file in files:
            if file.endswith(".rs"):
                file_path = os.path.join(root, file)
                try:
                    with open(file_path, 'r', encoding='utf-8') as f:
                        # 读取文件的前几行（假设头信息在前 10 行内），以避免读取整个大文件
                        lines = [f.readline() for _ in range(10)]
                        content_head = "".join(lines)
                        
                        if EXPECTED_HEADER not in content_head:
                            missing_headers_list.append(file_path)
                except Exception as e:
                    print(f"无法读取文件 {file_path}: {e}")
                    
    return missing_headers_list

def add_missing_headers(missing_list):
    """
    为缺失文件头的文件添加版权头信息。
    """
    if not missing_list:
        print("🎉 恭喜！所有的 .rs 文件都包含了指定的版权头信息。")
        return

    print(f"⚠️ 发现 {len(missing_list)} 个 .rs 文件缺失指定的版权头信息，正在添加...")
    print("-" * 50)
    for path in missing_list:
        try:
            with open(path, 'r', encoding='utf-8') as f:
                content = f.read()
            
            with open(path, 'w', encoding='utf-8') as f:
                f.write(EXPECTED_HEADER + "\n\n" + content)
            print(f"已添加: {path}")
        except Exception as e:
            print(f"❌ 无法处理文件 {path}: {e}")
    print("-" * 50)
    print("✅ 处理完成！")

if __name__ == "__main__":
    # 指定项目根目录运行，默认为当前目录 '.'
    missing_files = check_rs_file_headers(".")
    add_missing_headers(missing_files)