pub enum Slf {
    Rightleft = 1,
    Wrap = 2,
    IncVcol = 4,
}

pub enum RdbFlags {
    Compositor = 0x01,
    Nothrottle = 0x02,
    Invalid = 0x04,
    Nodelta = 0x08,
    Line = 0x10,
    Flush = 0x20,
}
