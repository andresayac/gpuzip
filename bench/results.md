System: NVIDIA GeForce RTX 5080, 617.14 · AMD Ryzen 7 9700X 8-Core Processor (16 threads) · 7-Zip 24.09 (x64) : Copyright (c) 1999-2024 Igor Pavlov : 2024-11-29 · nvCOMP 6.0.0.12 · Python 3.14.2

| Dataset | Job | 7-zip | Python cpu | Python gpu | Python auto | Rust cpu | Rust gpu | Rust auto | best vs 7-Zip |
|---|---|---|---|---|---|---|---|---|---|
| small | test | 4.78 s | 0.87 s | 1.79 s | 1.00 s | 0.28 s | 0.61 s | 0.39 s | 16.8x |
| small | extract | 26.81 s | 9.86 s | - | 11.20 s | 19.56 s | - | 19.77 s | 2.7x |
| medium | test | 5.08 s | 0.46 s | 2.03 s | 0.48 s | 0.26 s | 0.86 s | 0.35 s | 19.5x |
| medium | extract | 7.31 s | 1.08 s | - | 1.53 s | 1.56 s | - | 1.87 s | 6.8x |
| large | test | 3.66 s | 0.45 s | 42.23 s | 0.45 s | 0.27 s | 21.16 s | 0.27 s | 13.6x |
| large | extract | 4.01 s | 0.67 s | - | 0.66 s | 0.62 s | - | 1.45 s | 6.5x |
| mixed | test | 2.80 s | 0.46 s | 11.93 s | 0.51 s | 0.19 s | 5.77 s | 0.22 s | 15.0x |
| mixed | extract | 9.07 s | 2.89 s | - | 3.24 s | 5.38 s | - | 5.21 s | 3.1x |
