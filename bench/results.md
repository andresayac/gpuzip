System: NVIDIA GeForce RTX 5080, 617.14 · AMD Ryzen 7 9700X 8-Core Processor (16 threads) · 7-Zip 24.09 (x64) : Copyright (c) 1999-2024 Igor Pavlov : 2024-11-29 · nvCOMP 6.0.0.12 · Python 3.14.2

| Dataset | Job | 7-zip | Python cpu | Python gpu | Python auto | Rust cpu | Rust gpu | Rust auto | best vs 7-Zip |
|---|---|---|---|---|---|---|---|---|---|
| small | test | 4.34 s | 0.81 s | 1.64 s | 0.93 s | 0.26 s | 0.57 s | 0.26 s | 16.6x |
| small | extract | 25.54 s | 9.29 s | - | 9.30 s | 18.80 s | - | 19.36 s | 2.7x |
| medium | test | 5.02 s | 0.43 s | 1.96 s | 0.47 s | 0.26 s | 2.36 s | 0.26 s | 19.6x |
| medium | extract | 7.12 s | 1.10 s | - | 1.11 s | 1.48 s | - | 1.58 s | 6.5x |
| large | test | 3.63 s | 0.44 s | 43.29 s | 0.43 s | 0.26 s | 0.48 s | 0.26 s | 14.3x |
| large | extract | 3.90 s | 0.64 s | - | 0.67 s | 0.48 s | - | 0.48 s | 8.1x |
| mixed | test | 2.83 s | 0.43 s | 12.22 s | 0.52 s | 0.22 s | 0.85 s | 0.20 s | 13.9x |
| mixed | extract | 9.59 s | 2.83 s | - | 3.09 s | 5.41 s | - | 5.29 s | 3.4x |
