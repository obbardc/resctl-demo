use super::*;
use plotlib::page::Page;
use plotlib::repr::Plot;
use plotlib::style::{LineStyle, PointMarker, PointStyle};
use plotlib::view::ContinuousView;
use std::process::Command;

pub struct Grapher<'a> {
    out: Box<dyn Write + 'a>,
    file_prefix: Option<String>,
}

impl<'a> Grapher<'a> {
    pub fn new(out: Box<dyn Write + 'a>, file_prefix: Option<&str>) -> Self {
        Self {
            out,
            file_prefix: file_prefix.map(|x| x.to_owned()),
        }
    }

    fn setup_view(
        sel: &DataSel,
        series: &DataSeries,
        mem_profile: u32,
        isol_prot_pct: &str,
        extra_info: Option<&str>,
    ) -> (ContinuousView, f64, f64) {
        let (vrate_max, val_max, val_min) = series
            .points
            .iter()
            .chain(series.outliers.iter())
            .fold((0.0_f64, 0.0_f64, std::f64::MAX), |acc, point| {
                (acc.0.max(point.0), acc.1.max(point.1), acc.1.min(point.1))
            });

        let (ymin, yscale) = match sel {
            DataSel::MOF => {
                let ymin = if val_min >= 1.0 {
                    1.0
                } else {
                    val_min - (val_max - val_min) / 10.0
                };
                (ymin, 1.0)
            }
            DataSel::AMOF => {
                let ymin = if val_min >= 1.0 {
                    1.0
                } else {
                    val_min - (val_max - val_min) / 10.0
                };
                (ymin, 1.0)
            }
            DataSel::IsolProt => (0.0, 100.0),
            DataSel::IsolPct(_) => (0.0, 100.0),
            DataSel::Isol => (0.0, 100.0),
            DataSel::LatImp => (0.0, 100.0),
            DataSel::WorkCsv => (0.0, 100.0),
            DataSel::Missing => (0.0, 100.0),
            DataSel::RLat(_, _) => (0.0, 1000.0),
            DataSel::WLat(_, _) => (0.0, 1000.0),
        };
        let ymax = (val_max * 1.1).max((ymin) + 0.000001);

        let lines = &series.lines;
        let mut xlabel = format!(
            "vrate (min={:.3} max={:.3} ",
            lines.left.1.min(lines.right.1) * yscale,
            lines.left.1.max(lines.right.1) * yscale
        );
        if lines.left.0 > 0.0 {
            xlabel += &format!("left-infl={:.1} ", lines.left.0);
        }
        if lines.right.0 < vrate_max {
            xlabel += &format!("right-infl={:.1} ", lines.right.0);
        }
        xlabel += &format!("err={:.3})", series.error * yscale);

        let mut ylabel = match sel {
            DataSel::MOF | DataSel::AMOF => format!("{}@{}", sel, mem_profile),
            DataSel::IsolProt => format!("isol{}", isol_prot_pct),
            sel => format!("{}", sel),
        };
        if extra_info.is_some() {
            ylabel += &format!(" ({})", extra_info.as_ref().unwrap());
        }

        let view = ContinuousView::new()
            .x_range(0.0, (vrate_max * 1.1).max(0.000001))
            .y_range(ymin * yscale, ymax * yscale)
            .x_label(xlabel)
            .y_label(ylabel);

        (view, vrate_max, yscale)
    }

    fn plot_one_text(
        &mut self,
        sel: &DataSel,
        series: &DataSeries,
        mem_profile: u32,
        isol_prot_pct: &str,
    ) -> Result<()> {
        const SIZE: (u32, u32) = (80, 24);
        let (view, vrate_max, yscale) =
            Self::setup_view(sel, series, mem_profile, isol_prot_pct, None);

        let mut lines = vec![];
        for i in 0..SIZE.0 {
            let vrate = vrate_max / SIZE.0 as f64 * i as f64;
            lines.push((vrate, series.lines.eval(vrate) * yscale));
        }
        let view =
            view.add(Plot::new(lines).point_style(PointStyle::new().marker(PointMarker::Square)));

        let outliers = series
            .outliers
            .iter()
            .map(|(vrate, val)| (*vrate, val * yscale))
            .collect();
        let view =
            view.add(Plot::new(outliers).point_style(PointStyle::new().marker(PointMarker::Cross)));

        let points = series
            .points
            .iter()
            .map(|(vrate, val)| (*vrate, val * yscale))
            .collect();
        let view =
            view.add(Plot::new(points).point_style(PointStyle::new().marker(PointMarker::Circle)));

        let page = Page::single(&view).dimensions(SIZE.0, SIZE.1);
        write!(self.out, "{}\n\n", page.to_text().unwrap()).unwrap();
        Ok(())
    }

    fn plot_filename(&self, sel: &DataSel) -> String {
        format!("{}-{}.svg", self.file_prefix.as_ref().unwrap(), sel)
    }

    fn plot_one_svg(
        &mut self,
        sel: &DataSel,
        series: &DataSeries,
        mem_profile: u32,
        isol_prot_pct: &str,
        extra_info: &str,
    ) -> Result<()> {
        const SIZE: (u32, u32) = (576, 468);
        let (view, vrate_max, yscale) =
            Self::setup_view(sel, series, mem_profile, isol_prot_pct, Some(extra_info));

        let points = series
            .outliers
            .iter()
            .map(|(vrate, val)| (*vrate, val * yscale))
            .collect();
        let view = view.add(
            Plot::new(points).point_style(
                PointStyle::new()
                    .marker(PointMarker::Cross)
                    .colour("#37c0e6"),
            ),
        );

        let points = series
            .points
            .iter()
            .map(|(vrate, val)| (*vrate, val * yscale))
            .collect();
        let view = view.add(
            Plot::new(points).point_style(
                PointStyle::new()
                    .marker(PointMarker::Circle)
                    .colour("#37c0e6"),
            ),
        );

        let lines = &series.lines;
        let segments = vec![
            (0.0, lines.left.1 * yscale),
            (lines.left.0, lines.left.1 * yscale),
            (lines.right.0, lines.right.1 * yscale),
            (vrate_max, lines.right.1 * yscale),
        ];
        let view = view.add(Plot::new(segments).line_style(LineStyle::new().colour("#3749e6")));

        let view = view.x_max_ticks(10).y_max_ticks(10);

        if let Err(e) = Page::single(&view)
            .dimensions(SIZE.0, SIZE.1)
            .save(self.plot_filename(sel))
        {
            bail!("{}", &e);
        }
        Ok(())
    }

    fn collect_svgs(&self, sels: Vec<DataSel>, dst: &str) -> Result<()> {
        const NR_PER_PAGE: usize = 6;

        let groups = DataSel::align_and_merge_groups(DataSel::group(sels), NR_PER_PAGE);
        let mut srcs: Vec<String> = vec![];
        for grp in groups.iter() {
            srcs.extend(grp.iter().map(|sel| self.plot_filename(sel)));
            let pad = NR_PER_PAGE - (grp.len() % NR_PER_PAGE);
            if pad < NR_PER_PAGE {
                srcs.extend(std::iter::repeat("null:".to_owned()).take(pad));
            }
        }

        run_command(
            Command::new("montage")
                .args(&[
                    "-font",
                    "cantarell",
                    "-density",
                    "150",
                    "-tile",
                    "2x3",
                    "-geometry",
                    "+0+0",
                ])
                .args(srcs)
                .arg(dst),
            "are imagemagick and cantarell font available?",
        )
    }

    pub fn plot(&mut self, data: &JobData, res: &IoCostTuneResult) -> Result<()> {
        for (sel, series) in res.data.iter() {
            self.plot_one_text(sel, series, res.mem_profile, &res.isol_prot_pct)?;
        }
        if self.file_prefix.is_none() {
            return Ok(());
        }

        for (sel, series) in res.data.iter() {
            let sr = data.sysinfo.sysreqs_report.as_ref().unwrap();
            if let Err(e) = self.plot_one_svg(
                sel,
                series,
                res.mem_profile,
                &res.isol_prot_pct,
                &format!("{}", sr.scr_dev_model.trim()),
            ) {
                bail!(
                    "Failed to plot graph into {:?} ({})",
                    &self.plot_filename(sel),
                    &e
                );
            }
        }

        let sels = res.data.iter().map(|(sel, _)| sel).cloned().collect();
        let dst = format!("{}.pdf", self.file_prefix.as_ref().unwrap());
        self.collect_svgs(sels, &dst)
            .map_err(|e| anyhow!("Failed to collect graphs into {:?} ({})", &dst, &e))
    }
}