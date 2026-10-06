## contracts (tools/list as served)
| cand | tools | schema B | description B | contract B (name+desc+schema) | largest tool |
|---|---|---|---|---|---|
| A | 4 | 24,069 | 897 | 25,209 | brainprint.context 9,062 |
| B | 4 | 14,625 | 897 | 15,765 | brainprint.context 6,701 |
| C | 14 | 35,524 | 744 | 37,214 | brainprint.context_change 5,820 |
| D | 2 | 686 | 441 | 1,247 | brainprint.call 782 |
| E | 5 | 24,150 | 1,054 | 25,507 | brainprint.context 9,062 |

## workloads: round trips / lookups / expands / invalid / args B / result B (model-visible tool I/O)
| workload | A | B | C | D | E |
|---|---|---|---|---|---|
| W1 inspect | 2rt 0lk 0ex 0err 15,905B | 2rt 0lk 0ex 0err 16,281B | 2rt 0lk 0ex 0err 15,905B | 3rt 1lk 0ex 0err 20,154B | 4rt 0lk 2ex 0err 27,197B |
| W2 find>inspect | 4rt 0lk 0ex 0err 34,355B | 4rt 0lk 0ex 0err 35,107B | 4rt 0lk 0ex 0err 34,323B | 6rt 2lk 0ex 0err 42,750B | 6rt 0lk 2ex 0err 45,648B |
| W3 find>relations>inspect | 5rt 0lk 0ex 0err 41,479B | 5rt 0lk 0ex 0err 42,231B | 5rt 0lk 0ex 0err 41,431B | 8rt 3lk 0ex 0err 52,306B | 7rt 0lk 2ex 0err 52,772B |
| W4 change | 7rt 0lk 0ex 0err 49,614B | 7rt 0lk 0ex 0err 50,742B | 7rt 0lk 0ex 0err 49,534B | 11rt 4lk 0ex 0err 69,621B | 11rt 0lk 4ex 0err 67,019B |
| W5 resume | 3rt 0lk 0ex 0err 10,481B | 3rt 0lk 0ex 0err 10,481B | 3rt 0lk 0ex 0err 10,451B | 6rt 3lk 0ex 0err 19,827B | 4rt 0lk 1ex 0err 15,563B |
| W6 low-frequency | 2rt 0lk 0ex 0err 30,393B | 2rt 0lk 0ex 0err 30,393B | 2rt 0lk 0ex 0err 30,357B | 4rt 2lk 0ex 0err 34,492B | 2rt 0lk 0ex 0err 30,393B |
| W7 inspect x10 | 20rt 0lk 0ex 0err 159,050B | 20rt 0lk 0ex 0err 162,810B | 20rt 0lk 0ex 0err 159,050B | 21rt 1lk 0ex 0err 163,947B | 40rt 0lk 20ex 0err 271,970B |
| W8 mixed | 14rt 0lk 0ex 0err 92,841B | 14rt 0lk 0ex 0err 94,721B | 14rt 0lk 0ex 0err 92,685B | 22rt 8lk 0ex 0err 122,120B | 20rt 0lk 6ex 0err 122,641B |

## parity vs A (A-equivalent payloads equal / compared)
B {'W1 inspect': '2/2', 'W2 find>inspect': '4/4', 'W3 find>relations>inspect': '5/5', 'W4 change': '7/7', 'W5 resume': '3/3', 'W6 low-frequency': '2/2', 'W7 inspect x10': '20/20', 'W8 mixed': '14/14'}
C {'W1 inspect': '2/2', 'W2 find>inspect': '4/4', 'W3 find>relations>inspect': '5/5', 'W4 change': '7/7', 'W5 resume': '3/3', 'W6 low-frequency': '2/2', 'W7 inspect x10': '20/20', 'W8 mixed': '14/14'}
D {'W1 inspect': '2/2', 'W2 find>inspect': '4/4', 'W3 find>relations>inspect': '5/5', 'W4 change': '7/7', 'W5 resume': '3/3', 'W6 low-frequency': '2/2', 'W7 inspect x10': '20/20', 'W8 mixed': '14/14'}
E {'W1 inspect': '2/2', 'W2 find>inspect': '4/4', 'W3 find>relations>inspect': '5/5', 'W4 change': '7/7', 'W5 resume': '3/3', 'W6 low-frequency': '2/2', 'W7 inspect x10': '20/20', 'W8 mixed': '14/14'}

## E handle status on expand: ['unchanged']
