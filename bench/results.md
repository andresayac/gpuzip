System: NVIDIA GeForce RTX 5080, 617.14 · AMD Ryzen 7 9700X 8-Core Processor (16 threads) · 7-Zip 24.09 (x64) : Copyright (c) 1999-2024 Igor Pavlov : 2024-11-29 · nvCOMP 6.0.0 · Python 3.14.2

| Dataset | Job | 7-zip | Rust cpu | Rust gpu | Rust auto | best vs 7-Zip |
|---|---|---|---|---|---|---|
| small | test | 4.46 s | 0.28 s | 0.64 s | 0.29 s | 15.7x |
| small | extract | 30.00 s | 20.09 s | - | 20.69 s | 1.5x |
| medium | test | 5.00 s | 0.25 s | 2.38 s | 0.27 s | 19.9x |
| medium | extract | 7.42 s | 1.68 s | - | 1.67 s | 4.5x |
| large | test | 3.64 s | 0.27 s | 0.52 s | 0.27 s | 13.7x |
| large | extract | 3.97 s | 0.49 s | - | 0.49 s | 8.1x |
| mixed | test | 2.84 s | 0.20 s | 0.88 s | 0.20 s | 14.2x |
| mixed | extract | 9.47 s | 5.58 s | - | 5.17 s | 1.8x |
