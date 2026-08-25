import re

with open('src/pipelines/bug.rs', 'r') as f:
    text = f.read()

text = text.replace('You are a bug deduplication specialized model', 'expert bug deduplication')
text = text.replace('You are a specialized security researcher', 'You are a specialized security researcher')
text = text.replace('sys.contains("expert bug deduplication")', 'sys.contains("expert bug deduplication") || usr.contains("expert bug deduplication")')
text = text.replace('sys.contains("You are a specialized security researcher")', 'sys.contains("You are a specialized security researcher") || usr.contains("You are a specialized security researcher")')

with open('src/pipelines/bug.rs', 'w') as f:
    f.write(text)
