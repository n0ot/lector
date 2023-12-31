use phf::phf_map;

static COLORS: phf::Map<u8, &'static str> = phf_map! {
            0u8 => "Black",
            1u8 => "Maroon",
            2u8 => "Green",
            3u8 => "Olive",
            4u8 => "Navy",
            5u8 => "Purple",
            6u8 => "Teal",
            7u8 => "Silver",
            8u8 => "Grey",
            9u8 => "Red",
            10u8 => "Lime",
            11u8 => "Yellow",
            12u8 => "Blue",
            13u8 => "Fuchsia",
            14u8 => "Aqua",
            15u8 => "White",
            16u8 => "Grey0",
            17u8 => "NavyBlue",
            18u8 => "DarkBlue",
            19u8 => "Blue3",
            20u8 => "Blue3",
            21u8 => "Blue1",
            22u8 => "DarkGreen",
            23u8 => "DeepSkyBlue4",
            24u8 => "DeepSkyBlue4",
            25u8 => "DeepSkyBlue4",
            26u8 => "DodgerBlue3",
            27u8 => "DodgerBlue2",
            28u8 => "Green4",
            29u8 => "SpringGreen4",
            30u8 => "Turquoise4",
            31u8 => "DeepSkyBlue3",
            32u8 => "DeepSkyBlue3",
            33u8 => "DodgerBlue1",
            34u8 => "Green3",
            35u8 => "SpringGreen3",
            36u8 => "DarkCyan",
            37u8 => "LightSeaGreen",
            38u8 => "DeepSkyBlue2",
            39u8 => "DeepSkyBlue1",
            40u8 => "Green3",
            41u8 => "SpringGreen3",
            42u8 => "SpringGreen2",
            43u8 => "Cyan3",
            44u8 => "DarkTurquoise",
            45u8 => "Turquoise2",
            46u8 => "Green1",
            47u8 => "SpringGreen2",
            48u8 => "SpringGreen1",
            49u8 => "MediumSpringGreen",
            50u8 => "Cyan2",
            51u8 => "Cyan1",
            52u8 => "DarkRed",
            53u8 => "DeepPink4",
            54u8 => "Purple4",
            55u8 => "Purple4",
            56u8 => "Purple3",
            57u8 => "BlueViolet",
            58u8 => "Orange4",
            59u8 => "Grey37",
            60u8 => "MediumPurple4",
            61u8 => "SlateBlue3",
            62u8 => "SlateBlue3",
            63u8 => "RoyalBlue1",
            64u8 => "Chartreuse4",
            65u8 => "DarkSeaGreen4",
            66u8 => "PaleTurquoise4",
            67u8 => "SteelBlue",
            68u8 => "SteelBlue3",
            69u8 => "CornflowerBlue",
            70u8 => "Chartreuse3",
            71u8 => "DarkSeaGreen4",
            72u8 => "CadetBlue",
            73u8 => "CadetBlue",
            74u8 => "SkyBlue3",
            75u8 => "SteelBlue1",
            76u8 => "Chartreuse3",
            77u8 => "PaleGreen3",
            78u8 => "SeaGreen3",
            79u8 => "Aquamarine3",
            80u8 => "MediumTurquoise",
            81u8 => "SteelBlue1",
            82u8 => "Chartreuse2",
            83u8 => "SeaGreen2",
            84u8 => "SeaGreen1",
            85u8 => "SeaGreen1",
            86u8 => "Aquamarine1",
            87u8 => "DarkSlateGray2",
            88u8 => "DarkRed",
            89u8 => "DeepPink4",
            90u8 => "DarkMagenta",
            91u8 => "DarkMagenta",
            92u8 => "DarkViolet",
            93u8 => "Purple",
            94u8 => "Orange4",
            95u8 => "LightPink4",
            96u8 => "Plum4",
            97u8 => "MediumPurple3",
            98u8 => "MediumPurple3",
            99u8 => "SlateBlue1",
            100u8 => "Yellow4",
            101u8 => "Wheat4",
            102u8 => "Grey53",
            103u8 => "LightSlateGrey",
            104u8 => "MediumPurple",
            105u8 => "LightSlateBlue",
            106u8 => "Yellow4",
            107u8 => "DarkOliveGreen3",
            108u8 => "DarkSeaGreen",
            109u8 => "LightSkyBlue3",
            110u8 => "LightSkyBlue3",
            111u8 => "SkyBlue2",
            112u8 => "Chartreuse2",
            113u8 => "DarkOliveGreen3",
            114u8 => "PaleGreen3",
            115u8 => "DarkSeaGreen3",
            116u8 => "DarkSlateGray3",
            117u8 => "SkyBlue1",
            118u8 => "Chartreuse1",
            119u8 => "LightGreen",
            120u8 => "LightGreen",
            121u8 => "PaleGreen1",
            122u8 => "Aquamarine1",
            123u8 => "DarkSlateGray1",
            124u8 => "Red3",
            125u8 => "DeepPink4",
            126u8 => "MediumVioletRed",
            127u8 => "Magenta3",
            128u8 => "DarkViolet",
            129u8 => "Purple",
            130u8 => "DarkOrange3",
            131u8 => "IndianRed",
            132u8 => "HotPink3",
            133u8 => "MediumOrchid3",
            134u8 => "MediumOrchid",
            135u8 => "MediumPurple2",
            136u8 => "DarkGoldenrod",
            137u8 => "LightSalmon3",
            138u8 => "RosyBrown",
            139u8 => "Grey63",
            140u8 => "MediumPurple2",
            141u8 => "MediumPurple1",
            142u8 => "Gold3",
            143u8 => "DarkKhaki",
            144u8 => "NavajoWhite3",
            145u8 => "Grey69",
            146u8 => "LightSteelBlue3",
            147u8 => "LightSteelBlue",
            148u8 => "Yellow3",
            149u8 => "DarkOliveGreen3",
            150u8 => "DarkSeaGreen3",
            151u8 => "DarkSeaGreen2",
            152u8 => "LightCyan3",
            153u8 => "LightSkyBlue1",
            154u8 => "GreenYellow",
            155u8 => "DarkOliveGreen2",
            156u8 => "PaleGreen1",
            157u8 => "DarkSeaGreen2",
            158u8 => "DarkSeaGreen1",
            159u8 => "PaleTurquoise1",
            160u8 => "Red3",
            161u8 => "DeepPink3",
            162u8 => "DeepPink3",
            163u8 => "Magenta3",
            164u8 => "Magenta3",
            165u8 => "Magenta2",
            166u8 => "DarkOrange3",
            167u8 => "IndianRed",
            168u8 => "HotPink3",
            169u8 => "HotPink2",
            170u8 => "Orchid",
            171u8 => "MediumOrchid1",
            172u8 => "Orange3",
            173u8 => "LightSalmon3",
            174u8 => "LightPink3",
            175u8 => "Pink3",
            176u8 => "Plum3",
            177u8 => "Violet",
            178u8 => "Gold3",
            179u8 => "LightGoldenrod3",
            180u8 => "Tan",
            181u8 => "MistyRose3",
            182u8 => "Thistle3",
            183u8 => "Plum2",
            184u8 => "Yellow3",
            185u8 => "Khaki3",
            186u8 => "LightGoldenrod2",
            187u8 => "LightYellow3",
            188u8 => "Grey84",
            189u8 => "LightSteelBlue1",
            190u8 => "Yellow2",
            191u8 => "DarkOliveGreen1",
            192u8 => "DarkOliveGreen1",
            193u8 => "DarkSeaGreen1",
            194u8 => "Honeydew2",
            195u8 => "LightCyan1",
            196u8 => "Red1",
            197u8 => "DeepPink2",
            198u8 => "DeepPink1",
            199u8 => "DeepPink1",
            200u8 => "Magenta2",
            201u8 => "Magenta1",
            202u8 => "OrangeRed1",
            203u8 => "IndianRed1",
            204u8 => "IndianRed1",
            205u8 => "HotPink",
            206u8 => "HotPink",
            207u8 => "MediumOrchid1",
            208u8 => "DarkOrange",
            209u8 => "Salmon1",
            210u8 => "LightCoral",
            211u8 => "PaleVioletRed1",
            212u8 => "Orchid2",
            213u8 => "Orchid1",
            214u8 => "Orange1",
            215u8 => "SandyBrown",
            216u8 => "LightSalmon1",
            217u8 => "LightPink1",
            218u8 => "Pink1",
            219u8 => "Plum1",
            220u8 => "Gold1",
            221u8 => "LightGoldenrod2",
            222u8 => "LightGoldenrod2",
            223u8 => "NavajoWhite1",
            224u8 => "MistyRose1",
            225u8 => "Thistle1",
            226u8 => "Yellow1",
            227u8 => "LightGoldenrod1",
            228u8 => "Khaki1",
            229u8 => "Wheat1",
            230u8 => "Cornsilk1",
            231u8 => "Grey100",
            232u8 => "Grey3",
            233u8 => "Grey7",
            234u8 => "Grey11",
            235u8 => "Grey15",
            236u8 => "Grey19",
            237u8 => "Grey23",
            238u8 => "Grey27",
            239u8 => "Grey30",
            240u8 => "Grey35",
            241u8 => "Grey39",
            242u8 => "Grey42",
            243u8 => "Grey46",
            244u8 => "Grey50",
            245u8 => "Grey54",
            246u8 => "Grey58",
            247u8 => "Grey62",
            248u8 => "Grey66",
            249u8 => "Grey70",
            250u8 => "Grey74",
            251u8 => "Grey78",
            252u8 => "Grey82",
            253u8 => "Grey85",
            254u8 => "Grey89",
            255u8 => "Grey93",
};

static RGB_INDEX: phf::Map<&str, u8> = phf_map! {
    "#000000" => 0,
    "#000080" => 4,
    "#000087" => 18,
    "#0000af" => 19,
    "#0000d7" => 20,
    "#0000ff" => 12,
    "#00005f" => 17,
    "#008000" => 2,
    "#008080" => 6,
    "#008700" => 28,
    "#008787" => 30,
    "#0087af" => 31,
    "#0087d7" => 32,
    "#0087ff" => 33,
    "#00875f" => 29,
    "#00af00" => 34,
    "#00af87" => 36,
    "#00afaf" => 37,
    "#00afd7" => 38,
    "#00afff" => 39,
    "#00af5f" => 35,
    "#00d700" => 40,
    "#00d787" => 42,
    "#00d7af" => 43,
    "#00d7d7" => 44,
    "#00d7ff" => 45,
    "#00d75f" => 41,
    "#00ff00" => 10,
    "#00ff87" => 48,
    "#00ffaf" => 49,
    "#00ffd7" => 50,
    "#00ffff" => 14,
    "#00ff5f" => 47,
    "#005f00" => 22,
    "#005f87" => 24,
    "#005faf" => 25,
    "#005fd7" => 26,
    "#005fff" => 27,
    "#005f5f" => 23,
    "#6c6c6c" => 242,
    "#767676" => 243,
    "#800000" => 1,
    "#800080" => 5,
    "#808000" => 3,
    "#808080" => 244,
    "#870000" => 88,
    "#870087" => 90,
    "#8700af" => 91,
    "#8700d7" => 92,
    "#8700ff" => 93,
    "#87005f" => 89,
    "#878700" => 100,
    "#878787" => 102,
    "#8787af" => 103,
    "#8787d7" => 104,
    "#8787ff" => 105,
    "#87875f" => 101,
    "#87af00" => 106,
    "#87af87" => 108,
    "#87afaf" => 109,
    "#87afd7" => 110,
    "#87afff" => 111,
    "#87af5f" => 107,
    "#87d700" => 112,
    "#87d787" => 114,
    "#87d7af" => 115,
    "#87d7d7" => 116,
    "#87d7ff" => 117,
    "#87d75f" => 113,
    "#87ff00" => 118,
    "#87ff87" => 120,
    "#87ffaf" => 121,
    "#87ffd7" => 122,
    "#87ffff" => 123,
    "#87ff5f" => 119,
    "#875f00" => 94,
    "#875f87" => 96,
    "#875faf" => 97,
    "#875fd7" => 98,
    "#875fff" => 99,
    "#875f5f" => 95,
    "#8a8a8a" => 245,
    "#949494" => 246,
    "#9e9e9e" => 247,
    "#a8a8a8" => 248,
    "#af0000" => 124,
    "#af0087" => 126,
    "#af00af" => 127,
    "#af00d7" => 128,
    "#af00ff" => 129,
    "#af005f" => 125,
    "#af8700" => 136,
    "#af8787" => 138,
    "#af87af" => 139,
    "#af87d7" => 140,
    "#af87ff" => 141,
    "#af875f" => 137,
    "#afaf00" => 142,
    "#afaf87" => 144,
    "#afafaf" => 145,
    "#afafd7" => 146,
    "#afafff" => 147,
    "#afaf5f" => 143,
    "#afd700" => 148,
    "#afd787" => 150,
    "#afd7af" => 151,
    "#afd7d7" => 152,
    "#afd7ff" => 153,
    "#afd75f" => 149,
    "#afff00" => 154,
    "#afff87" => 156,
    "#afffaf" => 157,
    "#afffd7" => 158,
    "#afffff" => 159,
    "#afff5f" => 155,
    "#af5f00" => 130,
    "#af5f87" => 132,
    "#af5faf" => 133,
    "#af5fd7" => 134,
    "#af5fff" => 135,
    "#af5f5f" => 131,
    "#b2b2b2" => 249,
    "#121212" => 233,
    "#bcbcbc" => 250,
    "#c0c0c0" => 7,
    "#c6c6c6" => 251,
    "#d0d0d0" => 252,
    "#d70000" => 160,
    "#d70087" => 162,
    "#d700af" => 163,
    "#d700d7" => 164,
    "#d700ff" => 165,
    "#d7005f" => 161,
    "#d78700" => 172,
    "#d78787" => 174,
    "#d787af" => 175,
    "#d787d7" => 176,
    "#d787ff" => 177,
    "#d7875f" => 173,
    "#d7af00" => 178,
    "#d7af87" => 180,
    "#d7afaf" => 181,
    "#d7afd7" => 182,
    "#d7afff" => 183,
    "#d7af5f" => 179,
    "#d7d700" => 184,
    "#d7d787" => 186,
    "#d7d7af" => 187,
    "#d7d7d7" => 188,
    "#d7d7ff" => 189,
    "#d7d75f" => 185,
    "#d7ff00" => 190,
    "#d7ff87" => 192,
    "#d7ffaf" => 193,
    "#d7ffd7" => 194,
    "#d7ffff" => 195,
    "#d7ff5f" => 191,
    "#d75f00" => 166,
    "#d75f87" => 168,
    "#d75faf" => 169,
    "#d75fd7" => 170,
    "#d75fff" => 171,
    "#d75f5f" => 167,
    "#dadada" => 253,
    "#e4e4e4" => 254,
    "#eeeeee" => 255,
    "#ff0000" => 196,
    "#ff0087" => 198,
    "#ff00af" => 199,
    "#ff00d7" => 200,
    "#ff00ff" => 13,
    "#ff005f" => 197,
    "#ff8700" => 208,
    "#ff8787" => 210,
    "#ff87af" => 211,
    "#ff87d7" => 212,
    "#ff87ff" => 213,
    "#ff875f" => 209,
    "#ffaf00" => 214,
    "#ffaf87" => 216,
    "#ffafaf" => 217,
    "#ffafd7" => 218,
    "#ffafff" => 219,
    "#ffaf5f" => 215,
    "#ffd700" => 220,
    "#ffd787" => 222,
    "#ffd7af" => 223,
    "#ffd7d7" => 224,
    "#ffd7ff" => 225,
    "#ffd75f" => 221,
    "#ffff00" => 11,
    "#ffff87" => 228,
    "#ffffaf" => 229,
    "#ffffd7" => 230,
    "#ffffff" => 15,
    "#ffff5f" => 227,
    "#ff5f00" => 202,
    "#ff5f87" => 204,
    "#ff5faf" => 205,
    "#ff5fd7" => 206,
    "#ff5fff" => 207,
    "#ff5f5f" => 203,
    "#1c1c1c" => 234,
    "#262626" => 235,
    "#303030" => 236,
    "#3a3a3a" => 237,
    "#444444" => 238,
    "#4e4e4e" => 239,
    "#080808" => 232,
    "#585858" => 240,
    "#5f0000" => 52,
    "#5f0087" => 54,
    "#5f00af" => 55,
    "#5f00d7" => 56,
    "#5f00ff" => 57,
    "#5f005f" => 53,
    "#5f8700" => 64,
    "#5f8787" => 66,
    "#5f87af" => 67,
    "#5f87d7" => 68,
    "#5f87ff" => 69,
    "#5f875f" => 65,
    "#5faf00" => 70,
    "#5faf87" => 72,
    "#5fafaf" => 73,
    "#5fafd7" => 74,
    "#5fafff" => 75,
    "#5faf5f" => 71,
    "#5fd700" => 76,
    "#5fd787" => 78,
    "#5fd7af" => 79,
    "#5fd7d7" => 80,
    "#5fd7ff" => 81,
    "#5fd75f" => 77,
    "#5fff00" => 82,
    "#5fff87" => 84,
    "#5fffaf" => 85,
    "#5fffd7" => 86,
    "#5fffff" => 87,
    "#5fff5f" => 83,
    "#5f5f00" => 58,
    "#5f5f87" => 60,
    "#5f5faf" => 61,
    "#5f5fd7" => 62,
    "#5f5fff" => 63,
    "#5f5f5f" => 59,
    "#626262" => 241,
};

pub fn describe_color(color: vt100::Color) -> String {
    use vt100::Color::*;
    match color {
        Default => "default".into(),
        Idx(i) => match COLORS.get(&i) {
            Some(s) => s.to_string(),
            None => "unknown".into(),
        },
        Rgb(r, g, b) => {
            let rgb = format!("#{:02X}{:02X}{:02X}", r, g, b);
            match RGB_INDEX.get(&rgb).and_then(|i| COLORS.get(i)) {
                Some(s) => s.to_string(),
                None => rgb,
            }
        }
    }
}
