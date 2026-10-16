| Dataset | Job | Tool | Time | vs 7-Zip |
|---|---|---|---|---|
| text.gz | test | 7-Zip | 0.47 s | 1.0x |
| text.gz | test | GNU gzip | 0.62 s | 0.8x |
| text.gz | test | rapidgzip, 16 threads | 0.15 s | 3.0x |
| text.gz | test | gpuzip cpu | 0.24 s | 1.9x |
| text.gz | test | gpuzip gpu | 0.19 s | 2.5x |
| text.gz | test | gpuzip auto | 0.19 s | 2.5x |
| text.gz | extract | 7-Zip | 0.51 s | 1.0x |
| text.gz | extract | GNU gzip | 0.68 s | 0.7x |
| text.gz | extract | rapidgzip, 16 threads | 0.17 s | 3.1x |
| text.gz | extract | gpuzip cpu | 0.28 s | 1.8x |
| text.gz | extract | gpuzip auto | 0.22 s | 2.3x |
| big6.txt.gz | test | 7-Zip | 21.22 s | 1.0x |
| big6.txt.gz | test | GNU gzip | 28.50 s | 0.7x |
| big6.txt.gz | test | rapidgzip, 16 threads | 3.34 s | 6.4x |
| big6.txt.gz | test | gpuzip cpu | 10.63 s | 2.0x |
| big6.txt.gz | test | gpuzip gpu | 2.24 s | 9.5x |
| big6.txt.gz | test | gpuzip auto | 2.24 s | 9.5x |
| medium.tar.gz | test | 7-Zip | 4.67 s | 1.0x |
| medium.tar.gz | test | GNU gzip | 6.25 s | 0.7x |
| medium.tar.gz | test | rapidgzip, 16 threads | 0.82 s | 5.7x |
| medium.tar.gz | test | gpuzip cpu | 2.45 s | 1.9x |
| medium.tar.gz | test | gpuzip gpu | 0.70 s | 6.7x |
| medium.tar.gz | test | gpuzip auto | 0.70 s | 6.7x |
| medium.tar.gz | extract | 7-Zip (gz, then tar) | 7.08 s | 1.0x |
| medium.tar.gz | extract | Windows tar | 3.87 s | 1.8x |
| medium.tar.gz | extract | rapidgzip, then Windows tar | 2.87 s | 2.5x |
| medium.tar.gz | extract | gpuzip cpu | 2.83 s | 2.5x |
| medium.tar.gz | extract | gpuzip auto | 2.69 s | 2.6x |
