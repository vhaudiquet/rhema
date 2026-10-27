//! HCT — hue, chroma, tone — the color space behind Material's tonal
//! palettes, and the solver that rounds a requested (hue, chroma, tone)
//! to the nearest sRGB color. A trimmed port of Google's
//! material-color-utilities (Apache-2.0): CAM16 in the standard sRGB
//! viewing conditions, plus the bifrost gamut mapping the reference
//! solver uses. Only what the M3 You palette needs survives; keep the
//! formulas byte-faithful to the reference so its published values stay
//! reproducible.

use std::sync::OnceLock;

pub(crate) const XYZ_TO_CAM16RGB: [[f64; 3]; 3] = [
    [0.401288, 0.650173, -0.051461],
    [-0.250268, 1.204414, 0.045854],
    [-0.002079, 0.048952, 0.953127],
];

const SCALED_DISCOUNT_FROM_LINRGB: [[f64; 3]; 3] = [
    [
        0.001200833568784504,
        0.002389694492170889,
        0.0002795742885861124,
    ],
    [
        0.0005891086651375999,
        0.0029785502573438758,
        0.0003270666104008398,
    ],
    [
        0.00010146692491640572,
        0.0005364214359186694,
        0.0032979401770712076,
    ],
];

const LINRGB_FROM_SCALED_DISCOUNT: [[f64; 3]; 3] = [
    [1373.2198709594231, -1100.4251190754821, -7.278681089101213],
    [-271.815969077903, 559.6580465940733, -32.46047482791194],
    [1.9622899599665666, -57.173814538844006, 308.7233197812385],
];

const Y_FROM_LINRGB: [f64; 3] = [0.2126, 0.7152, 0.0722];

/// Lints of the sRGB cube, one entry per critical plane: delinearized
/// values the boundary bisection interpolates between. Digits exactly
/// as the reference publishes them.
const CRITICAL_PLANES: [f64; 255] = [
    0.015176349177441876,
    0.045529047532325624,
    0.07588174588720938,
    0.10623444424209313,
    0.13658714259697685,
    0.16693984095186062,
    0.19729253930674434,
    0.2276452376616281,
    0.2579979360165119,
    0.28835063437139563,
    0.3188300904430532,
    0.350925934958123,
    0.3848314933096426,
    0.42057480301049466,
    0.458183274052838,
    0.4976837250274023,
    0.5391024159806381,
    0.5824650784040898,
    0.6277969426914107,
    0.6751227633498623,
    0.7244668422128921,
    0.775853049866786,
    0.829304845476233,
    0.8848452951698498,
    0.942497089126609,
    1.0022825574869039,
    1.0642236851973577,
    1.1283421258858297,
    1.1946592148522128,
    1.2631959812511864,
    1.3339731595349034,
    1.407011200216447,
    1.4823302800086415,
    1.5599503113873272,
    1.6398909516233677,
    1.7221716113234105,
    1.8068114625156377,
    1.8938294463134073,
    1.9832442801866852,
    2.075074464868551,
    2.1693382909216234,
    2.2660538449872063,
    2.36523901573795,
    2.4669114995532007,
    2.5710888059345764,
    2.6777882626779785,
    2.7870270208169257,
    2.898822059350997,
    3.0131901897720907,
    3.1301480604002863,
    3.2497121605402226,
    3.3718988244681087,
    3.4967242352587946,
    3.624204428461639,
    3.754355295633311,
    3.887192587735158,
    4.022731918402185,
    4.160988767090289,
    4.301978482107941,
    4.445716283538092,
    4.592217266055746,
    4.741496401646282,
    4.893568542229298,
    5.048448422192488,
    5.20615066083972,
    5.3666897647573375,
    5.5300801301023865,
    5.696336044816294,
    5.865471690767354,
    6.037501145825082,
    6.212438385869475,
    6.390297286737924,
    6.571091626112461,
    6.7548350853498045,
    6.941541251256611,
    7.131223617812143,
    7.323895587840543,
    7.5195704746346665,
    7.7182615035334345,
    7.919981813454504,
    8.124744458384042,
    8.332562408825165,
    8.543448553206703,
    8.757415699253682,
    8.974476575321063,
    9.194643831691977,
    9.417930041841839,
    9.644347703669503,
    9.873909240696694,
    10.106627003236781,
    10.342513269534024,
    10.58158024687427,
    10.8238400726681,
    11.069304815507364,
    11.317986476196008,
    11.569896988756009,
    11.825048221409341,
    12.083451977536606,
    12.345119996613247,
    12.610063955123938,
    12.878295467455942,
    13.149826086772048,
    13.42466730586372,
    13.702830557985108,
    13.984327217668513,
    14.269168601521828,
    14.55736596900856,
    14.848930523210871,
    15.143873411576273,
    15.44220572664832,
    15.743938506781891,
    16.04908273684337,
    16.35764934889634,
    16.66964922287304,
    16.985093187232053,
    17.30399201960269,
    17.62635644741625,
    17.95219714852476,
    18.281524751807332,
    18.614349837764564,
    18.95068293910138,
    19.290534541298456,
    19.633915083172692,
    19.98083495742689,
    20.331304511189067,
    20.685334046541502,
    21.042933821039977,
    21.404114048223256,
    21.76888489811322,
    22.137256497705877,
    22.50923893145328,
    22.884842241736916,
    23.264076429332462,
    23.6469514538663,
    24.033477234264016,
    24.42366364919083,
    24.817520537484558,
    25.21505769858089,
    25.61628489293138,
    26.021211842414342,
    26.429848230738664,
    26.842203703840827,
    27.258287870275353,
    27.678110301598522,
    28.10168053274597,
    28.529008062403893,
    28.96010235337422,
    29.39497283293396,
    29.83362889318845,
    30.276079891419332,
    30.722335150426627,
    31.172403958865512,
    31.62629557157785,
    32.08401920991837,
    32.54558406207592,
    33.010999283389665,
    33.4802739966603,
    33.953417292456834,
    34.430438229418264,
    34.911345834551085,
    35.39614910352207,
    35.88485700094671,
    36.37747846067349,
    36.87402238606382,
    37.37449765026789,
    37.87891309649659,
    38.38727753828926,
    38.89959975977785,
    39.41588851594697,
    39.93615253289054,
    40.460400508064545,
    40.98864111053629,
    41.520882981230194,
    42.05713473317016,
    42.597404951718396,
    43.141702194811224,
    43.6900349931913,
    44.24241185063697,
    44.798841244188324,
    45.35933162437017,
    45.92389141541209,
    46.49252901546552,
    47.065252796817916,
    47.64207110610409,
    48.22299226451468,
    48.808024568002054,
    49.3971762874833,
    49.9904556690408,
    50.587870934119984,
    51.189430279724725,
    51.79514187861014,
    52.40501387947288,
    53.0190544071392,
    53.637271562750364,
    54.259673423945976,
    54.88626804504493,
    55.517063457223934,
    56.15206766869424,
    56.79128866487574,
    57.43473440856916,
    58.08241284012621,
    58.734331877617365,
    59.39049941699807,
    60.05092333227251,
    60.715611475655585,
    61.38457167773311,
    62.057811747619894,
    62.7353394731159,
    63.417162620860914,
    64.10328893648692,
    64.79372614476921,
    65.48848194977529,
    66.18756403501224,
    66.89098006357258,
    67.59873767827808,
    68.31084450182222,
    69.02730813691093,
    69.74813616640164,
    70.47333615344107,
    71.20291564160104,
    71.93688215501312,
    72.67524319850172,
    73.41800625771542,
    74.16517879925733,
    74.9167682708136,
    75.67278210128072,
    76.43322770089146,
    77.1981124613393,
    77.96744375590167,
    78.74122893956174,
    79.51947534912904,
    80.30219030335869,
    81.08938110306934,
    81.88105503125999,
    82.67721935322541,
    83.4778813166706,
    84.28304815182372,
    85.09272707154808,
    85.90692527145302,
    86.72564993000343,
    87.54890820862819,
    88.3767072518277,
    89.2090541872801,
    90.04595612594655,
    90.88742016217518,
    91.73345337380438,
    92.58406282226491,
    93.43925555268066,
    94.29903859396902,
    95.16341895893969,
    96.03240364439274,
    96.9059996312159,
    97.78421388448044,
    98.6670533535366,
    99.55452497210776,
];

/// sRGB linearization of one channel, into 0-100.
fn linearized(component: u8) -> f64 {
    let normalized = f64::from(component) / 255.0;
    if normalized <= 0.040449936 {
        normalized / 12.92 * 100.0
    } else {
        ((normalized + 0.055) / 1.055).powf(2.4) * 100.0
    }
}

/// Inverse of [`linearized`], rounding to a channel value.
fn delinearized(component: f64) -> u8 {
    let normalized = component / 100.0;
    let value = if normalized <= 0.0031308 {
        normalized * 12.92
    } else {
        1.055 * normalized.powf(1.0 / 2.4) - 0.055
    };
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn argb_from_rgb(red: u8, green: u8, blue: u8) -> u32 {
    0xFF00_0000 | (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
}

fn argb_from_linrgb(linrgb: [f64; 3]) -> u32 {
    argb_from_rgb(
        delinearized(linrgb[0]),
        delinearized(linrgb[1]),
        delinearized(linrgb[2]),
    )
}

/// CIE L* for a luminance Y in 0-100.
fn lstar_from_y(y: f64) -> f64 {
    fn lab_f(t: f64) -> f64 {
        const E: f64 = 216.0 / 24389.0;
        const KAPPA: f64 = 24389.0 / 27.0;
        if t > E {
            t.cbrt()
        } else {
            (KAPPA * t + 16.0) / 116.0
        }
    }
    lab_f(y / 100.0) * 116.0 - 16.0
}

/// Luminance Y in 0-100 for a CIE L*.
fn y_from_lstar(lstar: f64) -> f64 {
    fn lab_inv_f(ft: f64) -> f64 {
        const E: f64 = 216.0 / 24389.0;
        const KAPPA: f64 = 24389.0 / 27.0;
        let cubed = ft * ft * ft;
        if cubed > E {
            cubed
        } else {
            (116.0 * ft - 16.0) / KAPPA
        }
    }
    100.0 * lab_inv_f((lstar + 16.0) / 116.0)
}

fn argb_from_lstar(lstar: f64) -> u32 {
    let component = delinearized(y_from_lstar(lstar));
    argb_from_rgb(component, component, component)
}

fn sanitize_degrees(degrees: f64) -> f64 {
    let mut degrees = degrees % 360.0;
    if degrees < 0.0 {
        degrees += 360.0;
    }
    degrees
}

/// The CAM16 viewing environment. Only the standard sRGB one exists
/// here; the fields are the cached intermediates the conversions share.
struct ViewingConditions {
    aw: f64,
    nbb: f64,
    ncb: f64,
    c: f64,
    nc: f64,
    n: f64,
    rgb_d: [f64; 3],
    fl: f64,
    z: f64,
}

impl ViewingConditions {
    /// The standard sRGB viewing conditions: the D65 white point, an
    /// adapting luminance of 200/π·Y(50)/100, an L*-50 background, and
    /// average surround, illuminant discounting off.
    fn srgb() -> &'static ViewingConditions {
        static VC: OnceLock<ViewingConditions> = OnceLock::new();
        VC.get_or_init(|| {
            let white_point = [95.047, 100.0, 108.883];
            let adapting_luminance = 200.0 / std::f64::consts::PI * y_from_lstar(50.0) / 100.0;
            let background_lstar = 50.0_f64.max(0.1);
            let surround = 2.0;
            let r_w = white_point
                .iter()
                .zip(XYZ_TO_CAM16RGB[0])
                .map(|(w, m)| w * m)
                .sum::<f64>();
            let g_w = white_point
                .iter()
                .zip(XYZ_TO_CAM16RGB[1])
                .map(|(w, m)| w * m)
                .sum::<f64>();
            let b_w = white_point
                .iter()
                .zip(XYZ_TO_CAM16RGB[2])
                .map(|(w, m)| w * m)
                .sum::<f64>();
            let f = 0.8 + surround / 10.0;
            let c = if f >= 0.9 {
                0.59 + (0.69 - 0.59) * (f - 0.9) * 10.0
            } else {
                0.525 + (0.59 - 0.525) * (f - 0.8) * 10.0
            };
            let d = (f * (1.0 - (1.0 / 3.6) * ((-adapting_luminance - 42.0) / 92.0).exp()))
                .clamp(0.0, 1.0);
            let nc = f;
            let rgb_d = [
                d * (100.0 / r_w) + 1.0 - d,
                d * (100.0 / g_w) + 1.0 - d,
                d * (100.0 / b_w) + 1.0 - d,
            ];
            let k = 1.0 / (5.0 * adapting_luminance + 1.0);
            let k4 = k * k * k * k;
            let k4_f = 1.0 - k4;
            let fl =
                (k4 * adapting_luminance) + (0.1 * k4_f * k4_f * (5.0 * adapting_luminance).cbrt());
            let n = y_from_lstar(background_lstar) / white_point[1];
            let z = 1.48 + n.sqrt();
            let nbb = 0.725 / n.powf(0.2);
            let ncb = nbb;
            let rgb_a = [
                (fl * rgb_d[0] * r_w / 100.0).powf(0.42),
                (fl * rgb_d[1] * g_w / 100.0).powf(0.42),
                (fl * rgb_d[2] * b_w / 100.0).powf(0.42),
            ];
            let rgb_a = [
                (400.0 * rgb_a[0]) / (rgb_a[0] + 27.13),
                (400.0 * rgb_a[1]) / (rgb_a[1] + 27.13),
                (400.0 * rgb_a[2]) / (rgb_a[2] + 27.13),
            ];
            let aw = ((2.0 * rgb_a[0]) + rgb_a[1] + (0.05 * rgb_a[2])) * nbb;
            ViewingConditions {
                aw,
                nbb,
                ncb,
                c,
                nc,
                n,
                rgb_d,
                fl,
                z,
            }
        })
    }
}

/// A CAM16 color: the two coordinates the palette math reads.
struct Cam16 {
    hue: f64,
    chroma: f64,
}

impl Cam16 {
    /// CAM16 of an sRGB color in the standard viewing conditions.
    fn from_argb(argb: u32) -> Cam16 {
        let r = linearized(((argb >> 16) & 0xFF) as u8);
        let g = linearized(((argb >> 8) & 0xFF) as u8);
        let b = linearized((argb & 0xFF) as u8);
        let x = 0.41233895 * r + 0.35762064 * g + 0.18051042 * b;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = 0.01932141 * r + 0.11916382 * g + 0.95034478 * b;
        let vc = ViewingConditions::srgb();

        let r_t = x * XYZ_TO_CAM16RGB[0][0] + y * XYZ_TO_CAM16RGB[0][1] + z * XYZ_TO_CAM16RGB[0][2];
        let g_t = x * XYZ_TO_CAM16RGB[1][0] + y * XYZ_TO_CAM16RGB[1][1] + z * XYZ_TO_CAM16RGB[1][2];
        let b_t = x * XYZ_TO_CAM16RGB[2][0] + y * XYZ_TO_CAM16RGB[2][1] + z * XYZ_TO_CAM16RGB[2][2];

        let r_d = vc.rgb_d[0] * r_t;
        let g_d = vc.rgb_d[1] * g_t;
        let b_d = vc.rgb_d[2] * b_t;

        let r_a_f = (vc.fl * r_d.abs() / 100.0).powf(0.42);
        let g_a_f = (vc.fl * g_d.abs() / 100.0).powf(0.42);
        let b_a_f = (vc.fl * b_d.abs() / 100.0).powf(0.42);
        let r_a = r_d.signum() * 400.0 * r_a_f / (r_a_f + 27.13);
        let g_a = g_d.signum() * 400.0 * g_a_f / (g_a_f + 27.13);
        let b_a = b_d.signum() * 400.0 * b_a_f / (b_a_f + 27.13);

        let a = (11.0 * r_a + -12.0 * g_a + b_a) / 11.0;
        let b = (r_a + g_a - 2.0 * b_a) / 9.0;
        let u = (20.0 * r_a + 20.0 * g_a + 21.0 * b_a) / 20.0;
        let p2 = (40.0 * r_a + 20.0 * g_a + b_a) / 20.0;

        let atan_degrees = b.atan2(a).to_degrees();
        let hue = if atan_degrees < 0.0 {
            atan_degrees + 360.0
        } else {
            atan_degrees
        };

        let ac = p2 * vc.nbb;
        let j = 100.0 * (ac / vc.aw).powf(vc.c * vc.z);

        let hue_prime = if hue < 20.14 { hue + 360.0 } else { hue };
        let e_hue = 0.25 * ((hue_prime.to_radians() + 2.0).cos() + 3.8);
        let p1 = 50000.0 / 13.0 * e_hue * vc.nc * vc.ncb;
        let t = p1 * a.hypot(b) / (u + 0.305);
        let alpha = (1.64 - 0.29_f64.powf(vc.n)).powf(0.73) * t.powf(0.9);
        let chroma = alpha * (j / 100.0).sqrt();

        Cam16 { hue, chroma }
    }
}

/// An HCT color: where it landed in sRGB and the coordinates that name it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Hct {
    /// Hue in degrees, 0-360.
    pub hue: f64,
    /// Chroma; the solver clamps this into the sRGB gamut.
    pub chroma: f64,
    /// CIE L* tone, 0-100.
    pub tone: f64,
    argb: u32,
}

impl Hct {
    /// HCT of an sRGB color (alpha ignored).
    pub fn from_argb(argb: u32) -> Hct {
        let cam = Cam16::from_argb(argb);
        let r = linearized(((argb >> 16) & 0xFF) as u8);
        let g = linearized(((argb >> 8) & 0xFF) as u8);
        let b = linearized((argb & 0xFF) as u8);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        Hct {
            hue: cam.hue,
            chroma: cam.chroma,
            tone: lstar_from_y(y),
            argb: 0xFF00_0000 | (argb & 0x00FF_FFFF),
        }
    }

    /// The sRGB color at `hue`/`chroma`/`tone`, chroma clamped into the
    /// gamut.
    pub fn from(hue: f64, chroma: f64, tone: f64) -> Hct {
        Hct::from_argb(solve_to_int(hue, chroma, tone))
    }

    /// The sRGB color as full ARGB, alpha 255.
    pub fn argb(self) -> u32 {
        self.argb
    }
}

/// A tonal palette: hues and chromas held constant, tones stepped.
pub(crate) struct TonalPalette {
    hue: f64,
    chroma: f64,
}

impl TonalPalette {
    /// The palette at `hue` and `chroma`.
    pub fn new(hue: f64, chroma: f64) -> TonalPalette {
        TonalPalette { hue, chroma }
    }

    /// The sRGB color at `tone`; the solver clamps the palette chroma
    /// where the gamut runs out.
    pub fn tone(&self, tone: u8) -> u32 {
        Hct::from(self.hue, self.chroma, f64::from(tone)).argb()
    }
}

/// Finds an sRGB color with the given hue, chroma, and L*.
fn solve_to_int(hue_degrees: f64, chroma: f64, lstar: f64) -> u32 {
    if chroma < 0.0001 || !(0.0001..=99.9999).contains(&lstar) {
        return argb_from_lstar(lstar);
    }
    let hue_radians = sanitize_degrees(hue_degrees) / 180.0 * std::f64::consts::PI;
    let y = y_from_lstar(lstar);
    let exact_answer = find_result_by_j(hue_radians, chroma, y);
    if exact_answer != 0 {
        return exact_answer;
    }
    let linrgb = bisect_to_limit(y, hue_radians);
    argb_from_linrgb(linrgb)
}

fn sanitize_radians(angle: f64) -> f64 {
    (angle + std::f64::consts::PI * 8.0) % (std::f64::consts::PI * 2.0)
}

fn chromatic_adaptation(component: f64) -> f64 {
    let af = component.abs().powf(0.42);
    component.signum() * 400.0 * af / (af + 27.13)
}

/// The hue of a linear RGB color in CAM16, in radians.
fn hue_of(linrgb: [f64; 3]) -> f64 {
    let scaled_discount = [
        linrgb[0] * SCALED_DISCOUNT_FROM_LINRGB[0][0]
            + linrgb[1] * SCALED_DISCOUNT_FROM_LINRGB[0][1]
            + linrgb[2] * SCALED_DISCOUNT_FROM_LINRGB[0][2],
        linrgb[0] * SCALED_DISCOUNT_FROM_LINRGB[1][0]
            + linrgb[1] * SCALED_DISCOUNT_FROM_LINRGB[1][1]
            + linrgb[2] * SCALED_DISCOUNT_FROM_LINRGB[1][2],
        linrgb[0] * SCALED_DISCOUNT_FROM_LINRGB[2][0]
            + linrgb[1] * SCALED_DISCOUNT_FROM_LINRGB[2][1]
            + linrgb[2] * SCALED_DISCOUNT_FROM_LINRGB[2][2],
    ];
    let r_a = chromatic_adaptation(scaled_discount[0]);
    let g_a = chromatic_adaptation(scaled_discount[1]);
    let b_a = chromatic_adaptation(scaled_discount[2]);
    let a = (11.0 * r_a + -12.0 * g_a + b_a) / 11.0;
    let b = (r_a + g_a - 2.0 * b_a) / 9.0;
    b.atan2(a)
}

fn are_in_cyclic_order(a: f64, b: f64, c: f64) -> bool {
    sanitize_radians(b - a) < sanitize_radians(c - a)
}

fn lerp_point(source: [f64; 3], t: f64, target: [f64; 3]) -> [f64; 3] {
    [
        source[0] + (target[0] - source[0]) * t,
        source[1] + (target[1] - source[1]) * t,
        source[2] + (target[2] - source[2]) * t,
    ]
}

fn set_coordinate(source: [f64; 3], coordinate: f64, target: [f64; 3], axis: usize) -> [f64; 3] {
    let t = (coordinate - source[axis]) / (target[axis] - source[axis]);
    lerp_point(source, t, target)
}

fn is_bounded(x: f64) -> bool {
    (0.0..=100.0).contains(&x)
}

/// The nth possible vertex of the polygonal intersection.
fn nth_vertex(y: f64, n: usize) -> [f64; 3] {
    let (k_r, k_g, k_b) = (Y_FROM_LINRGB[0], Y_FROM_LINRGB[1], Y_FROM_LINRGB[2]);
    let coord_a = if n % 4 <= 1 { 0.0 } else { 100.0 };
    let coord_b = if n.is_multiple_of(2) { 0.0 } else { 100.0 };
    let (answer, channel) = if n < 4 {
        let g = coord_a;
        let b = coord_b;
        ((y - g * k_g - b * k_b) / k_r, 0)
    } else if n < 8 {
        let b = coord_a;
        let r = coord_b;
        ((y - r * k_r - b * k_b) / k_g, 1)
    } else {
        let r = coord_a;
        let g = coord_b;
        ((y - r * k_r - g * k_g) / k_b, 2)
    };
    if is_bounded(answer) {
        let mut vertex = [coord_b, coord_b, coord_b];
        vertex[channel] = answer;
        if n < 4 {
            vertex[1] = coord_a;
            vertex[2] = coord_b;
        } else if n < 8 {
            vertex[0] = coord_b;
            vertex[2] = coord_a;
        } else {
            vertex[0] = coord_a;
            vertex[1] = coord_b;
        }
        vertex
    } else {
        [-1.0, -1.0, -1.0]
    }
}

/// Finds the segment containing the desired color.
fn bisect_to_segment(y: f64, target_hue: f64) -> [[f64; 3]; 2] {
    let mut left = [-1.0, -1.0, -1.0];
    let mut right = left;
    let mut left_hue = 0.0;
    let mut right_hue = 0.0;
    let mut initialized = false;
    let mut uncut = true;
    for n in 0..12 {
        let mid = nth_vertex(y, n);
        if mid[0] < 0.0 {
            continue;
        }
        let mid_hue = hue_of(mid);
        if !initialized {
            left = mid;
            right = mid;
            left_hue = mid_hue;
            right_hue = mid_hue;
            initialized = true;
            continue;
        }
        if uncut || are_in_cyclic_order(left_hue, mid_hue, right_hue) {
            uncut = false;
            if are_in_cyclic_order(left_hue, target_hue, mid_hue) {
                right = mid;
                right_hue = mid_hue;
            } else {
                left = mid;
                left_hue = mid_hue;
            }
        }
    }
    [left, right]
}

fn critical_plane_below(x: f64) -> isize {
    (x - 0.5).floor() as isize
}

fn critical_plane_above(x: f64) -> isize {
    (x - 0.5).ceil() as isize
}

/// Finds a color with the given Y and hue on the boundary of the cube.
fn bisect_to_limit(y: f64, target_hue: f64) -> [f64; 3] {
    let segment = bisect_to_segment(y, target_hue);
    let mut left = segment[0];
    let mut left_hue = hue_of(left);
    let mut right = segment[1];
    for axis in 0..=2 {
        if left[axis] != right[axis] {
            let (mut l_plane, mut r_plane) = if left[axis] < right[axis] {
                (
                    critical_plane_below(delinearized_f64(left[axis])),
                    critical_plane_above(delinearized_f64(right[axis])),
                )
            } else {
                (
                    critical_plane_above(delinearized_f64(left[axis])),
                    critical_plane_below(delinearized_f64(right[axis])),
                )
            };
            for _ in 0..8 {
                if (r_plane - l_plane).abs() <= 1 {
                    break;
                }
                let m_plane = ((l_plane + r_plane) as f64 / 2.0).floor() as isize;
                let mid_plane_coordinate = CRITICAL_PLANES[m_plane as usize];
                let mid = set_coordinate(left, mid_plane_coordinate, right, axis);
                let mid_hue = hue_of(mid);
                if are_in_cyclic_order(left_hue, target_hue, mid_hue) {
                    right = mid;
                    r_plane = m_plane;
                } else {
                    left = mid;
                    left_hue = mid_hue;
                    l_plane = m_plane;
                }
            }
        }
    }
    [
        (left[0] + right[0]) / 2.0,
        (left[1] + right[1]) / 2.0,
        (left[2] + right[2]) / 2.0,
    ]
}

/// Delinearizes without rounding: the bisection interpolates on the
/// unrounded planes.
fn delinearized_f64(rgb_component: f64) -> f64 {
    let normalized = rgb_component / 100.0;
    let value = if normalized <= 0.0031308 {
        normalized * 12.92
    } else {
        1.055 * normalized.powf(1.0 / 2.4) - 0.055
    };
    value * 255.0
}

fn inverse_chromatic_adaptation(adapted: f64) -> f64 {
    let adapted_abs = adapted.abs();
    let base = (27.13 * adapted_abs / (400.0 - adapted_abs)).max(0.0);
    adapted.signum() * base.powf(1.0 / 0.42)
}

/// Finds a color with the given hue, chroma, and Y, by Newton iteration
/// on the CAM16 lightness; 0 when none exists in the gamut.
fn find_result_by_j(hue_radians: f64, chroma: f64, y: f64) -> u32 {
    let mut j = y.sqrt() * 11.0;
    let vc = ViewingConditions::srgb();
    let t_inner_coeff = 1.0 / (1.64 - 0.29_f64.powf(vc.n)).powf(0.73);
    let e_hue = 0.25 * ((hue_radians + 2.0).cos() + 3.8);
    let p1 = e_hue * (50000.0 / 13.0) * vc.nc * vc.ncb;
    let h_sin = hue_radians.sin();
    let h_cos = hue_radians.cos();

    for iteration_round in 0..5 {
        let j_normalized = j / 100.0;
        let alpha = if chroma == 0.0 || j == 0.0 {
            0.0
        } else {
            chroma / j_normalized.sqrt()
        };
        let t = (alpha * t_inner_coeff).powf(1.0 / 0.9);
        let ac = vc.aw * j_normalized.powf(1.0 / vc.c / vc.z);
        let p2 = ac / vc.nbb;
        let gamma = 23.0 * (p2 + 0.305) * t / (23.0 * p1 + 11.0 * t * h_cos + 108.0 * t * h_sin);
        let a = gamma * h_cos;
        let b = gamma * h_sin;
        let r_a = (460.0 * p2 + 451.0 * a + 288.0 * b) / 1403.0;
        let g_a = (460.0 * p2 - 891.0 * a - 261.0 * b) / 1403.0;
        let b_a = (460.0 * p2 - 220.0 * a - 6300.0 * b) / 1403.0;
        let r_c_scaled = inverse_chromatic_adaptation(r_a);
        let g_c_scaled = inverse_chromatic_adaptation(g_a);
        let b_c_scaled = inverse_chromatic_adaptation(b_a);
        let linrgb = [
            r_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[0][0]
                + g_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[0][1]
                + b_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[0][2],
            r_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[1][0]
                + g_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[1][1]
                + b_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[1][2],
            r_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[2][0]
                + g_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[2][1]
                + b_c_scaled * LINRGB_FROM_SCALED_DISCOUNT[2][2],
        ];
        if linrgb[0] < 0.0 || linrgb[1] < 0.0 || linrgb[2] < 0.0 {
            return 0;
        }
        let fnj = Y_FROM_LINRGB[0] * linrgb[0]
            + Y_FROM_LINRGB[1] * linrgb[1]
            + Y_FROM_LINRGB[2] * linrgb[2];
        if fnj <= 0.0 {
            return 0;
        }
        if iteration_round == 4 || (fnj - y).abs() < 0.002 {
            if linrgb[0] > 100.01 || linrgb[1] > 100.01 || linrgb[2] > 100.01 {
                return 0;
            }
            return argb_from_linrgb(linrgb);
        }
        // Newton iteration, with 2·fn(j)/j approximating fn'(j).
        j -= (fnj - y) * j / (2.0 * fnj);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solves_hue_chroma_tone() {
        // The reference solvers agree on this point (checked against
        // three independent ports of material-color-utilities).
        assert_eq!(Hct::from(209.0, 77.0, 50.0).argb(), 0xFF008491);
    }

    #[test]
    fn tonal_palette_of_blue_matches_the_reference() {
        let blue = TonalPalette::new(
            Hct::from_argb(0x0000FF).hue,
            Hct::from_argb(0x0000FF).chroma,
        );
        assert_eq!(blue.tone(95), 0xFFF1EFFF);
        assert_eq!(blue.tone(90), 0xFFE0E0FF);
        assert_eq!(blue.tone(80), 0xFFBEC2FF);
        assert_eq!(blue.tone(40), 0xFF343DFF);
        assert_eq!(blue.tone(30), 0xFF0000EF);
        assert_eq!(blue.tone(20), 0xFF0001AC);
        assert_eq!(blue.tone(10), 0xFF00006E);
    }

    #[test]
    fn round_trips_the_srgb_cube() {
        // A requested (hue, chroma, tone) solved back to sRGB must name
        // those same coordinates, wherever the gamut let it land.
        for r in (0..=255).step_by(41) {
            for g in (0..=255).step_by(41) {
                for b in (0..=255).step_by(41) {
                    let argb = argb_from_rgb(r, g, b);
                    let hct = Hct::from_argb(argb);
                    assert_eq!(
                        Hct::from(hct.hue, hct.chroma, hct.tone).argb(),
                        argb,
                        "rgb({r}, {g}, {b})"
                    );
                }
            }
        }
    }
}
